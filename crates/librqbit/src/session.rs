use std::{fs::File, io::Read, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Context;
use buffers::ByteString;
use dht::{Dht, Id20, PersistentDht, PersistentDhtConfig};
use librqbit_core::{
    magnet::Magnet,
    peer_id::generate_peer_id,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Info, TorrentMetaV1Owned},
};
use log::{info, warn};
use reqwest::Url;
use tokio_stream::StreamExt;

use size_format::SizeFormatterBinary as SF;

use crate::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    http_api::HttpApi,
    peer_connection::PeerConnectionOptions,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_manager::{TorrentManagerBuilder, TorrentManagerHandle},
};

pub struct Session {
    peer_id: Id20,
    dht: Option<Dht>,
    http_api: Option<HttpApi>,
    peer_opts: PeerConnectionOptions,
    spawner: BlockingSpawner,
    output_folder: PathBuf,
}

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("error downloading torrent metadata from {}", url))?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading repsonse body from {}", url))?;
    torrent_from_bytes(&b).context("error decoding torrent")
}

fn torrent_from_file(filename: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let mut buf = Vec::new();
    if filename == "-" {
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("error reading stdin")?;
    } else {
        File::open(filename)
            .with_context(|| format!("error opening {}", filename))?
            .read_to_end(&mut buf)
            .with_context(|| format!("error reading {}", filename))?;
    }
    torrent_from_bytes(&buf).context("error decoding torrent")
}

fn compute_only_files<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
    filename_re: &str,
) -> anyhow::Result<Vec<usize>> {
    let filename_re = regex::Regex::new(filename_re).context("filename regex is incorrect")?;
    let mut only_files = Vec::new();
    for (idx, (filename, _)) in torrent.iter_filenames_and_lengths()?.enumerate() {
        let full_path = filename
            .to_pathbuf()
            .with_context(|| format!("filename of file {} is not valid utf8", idx))?;
        if filename_re.is_match(full_path.to_str().unwrap()) {
            only_files.push(idx);
        }
    }
    if only_files.is_empty() {
        anyhow::bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

#[derive(Default)]
pub struct AddTorrentOptions {
    pub only_files_regex: Option<String>,
    pub overwrite: bool,
    pub list_only: bool,
    pub output_folder: Option<String>,
    pub peer_opts: Option<PeerConnectionOptions>,
    pub force_tracker_interval: Option<Duration>,
}

#[derive(Default)]
pub struct SessionOptions {
    pub disable_dht: bool,
    pub disable_dht_persistence: bool,
    pub dht_config: Option<PersistentDhtConfig>,
    pub disable_http_api: bool,
    pub http_api_listen_addr: Option<SocketAddr>,
    pub peer_id: Option<Id20>,
    pub peer_opts: Option<PeerConnectionOptions>,
}

impl Session {
    pub async fn new(output_folder: PathBuf, spawner: BlockingSpawner) -> anyhow::Result<Self> {
        Self::new_with_opts(output_folder, spawner, SessionOptions::default()).await
    }
    pub async fn new_with_opts(
        output_folder: PathBuf,
        spawner: BlockingSpawner,
        opts: SessionOptions,
    ) -> anyhow::Result<Self> {
        let peer_id = opts.peer_id.unwrap_or_else(generate_peer_id);
        let dht = if opts.disable_dht {
            None
        } else {
            let dht = if opts.disable_dht_persistence {
                Dht::new().await
            } else {
                PersistentDht::create(opts.dht_config).await
            }
            .context("error initializing DHT")?;
            Some(dht)
        };
        let peer_opts = opts.peer_opts.unwrap_or_default();

        let http_api = if opts.disable_http_api {
            None
        } else {
            let http_api_listen_addr = opts
                .http_api_listen_addr
                .unwrap_or_else(|| "127.0.0.1:3001".parse().unwrap());
            let http_api = HttpApi::new(dht.clone());
            spawn("HTTP API", {
                let http_api = http_api.clone();
                async move { http_api.make_http_api_and_run(http_api_listen_addr).await }
            });
            Some(http_api)
        };

        Ok(Self {
            peer_id,
            dht,
            http_api,
            peer_opts,
            spawner,
            output_folder,
        })
    }
    pub async fn add_torrent(
        &self,
        url: String,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<Option<TorrentManagerHandle>> {
        // Magnet links are different in that we first need to discover the metadata.
        let opts = opts.unwrap_or_default();
        if url.starts_with("magnet:") {
            let Magnet {
                info_hash,
                trackers,
            } = Magnet::parse(&url).context("provided path is not a valid magnet URL")?;

            let dht_rx = self
                .dht
                .as_ref()
                .context("magnet links without DHT are not supported")?
                .get_peers(info_hash)
                .await?;

            let trackers = trackers
                .into_iter()
                .filter_map(|url| match reqwest::Url::parse(&url) {
                    Ok(url) => Some(url),
                    Err(e) => {
                        warn!("error parsing tracker {} as url: {}", url, e);
                        None
                    }
                })
                .collect();

            let (info, dht_rx, initial_peers) = match read_metainfo_from_peer_receiver(
                self.peer_id,
                info_hash,
                dht_rx,
                Some(self.peer_opts),
            )
            .await
            {
                ReadMetainfoResult::Found { info, rx, seen } => (info, rx, seen),
                ReadMetainfoResult::ChannelClosed { .. } => {
                    anyhow::bail!("DHT died, no way to discover torrent metainfo")
                }
            };
            self.main_torrent_info(
                info_hash,
                info,
                Some(dht_rx),
                initial_peers.into_iter().collect(),
                trackers,
                opts,
            )
            .await
        } else {
            let torrent = if url.starts_with("http://") || url.starts_with("https://") {
                torrent_from_url(&url).await?
            } else {
                torrent_from_file(&url)?
            };
            let dht_rx = match self.dht.as_ref() {
                Some(dht) => Some(dht.get_peers(torrent.info_hash).await?),
                None => None,
            };
            let trackers = torrent
                .iter_announce()
                .filter_map(|tracker| {
                    let url = match std::str::from_utf8(tracker.as_ref()) {
                        Ok(url) => url,
                        Err(_) => {
                            warn!("cannot parse tracker url as utf-8, ignoring");
                            return None;
                        }
                    };
                    match Url::parse(url) {
                        Ok(url) => Some(url),
                        Err(e) => {
                            warn!("cannot parse tracker URL {}: {}", url, e);
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();
            self.main_torrent_info(
                torrent.info_hash,
                torrent.info,
                dht_rx,
                Vec::new(),
                trackers,
                opts,
            )
            .await
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn main_torrent_info(
        &self,
        info_hash: Id20,
        info: TorrentMetaV1Info<ByteString>,
        dht_peer_rx: Option<impl StreamExt<Item = SocketAddr> + Unpin + Send + Sync + 'static>,
        initial_peers: Vec<SocketAddr>,
        trackers: Vec<reqwest::Url>,
        opts: AddTorrentOptions,
    ) -> anyhow::Result<Option<TorrentManagerHandle>> {
        info!("Torrent info: {:#?}", &info);
        let only_files = if let Some(filename_re) = opts.only_files_regex {
            let only_files = compute_only_files(&info, &filename_re)?;
            for (idx, (filename, _)) in info.iter_filenames_and_lengths()?.enumerate() {
                if !only_files.contains(&idx) {
                    continue;
                }
                info!("Will download {:?}", filename);
            }
            Some(only_files)
        } else {
            None
        };

        if opts.list_only {
            for (idx, (filename, len)) in info.iter_filenames_and_lengths()?.enumerate() {
                let included = match &only_files {
                    Some(files) => files.contains(&idx),
                    None => true,
                };
                info!(
                    "File {}, size {}{}",
                    filename.to_string()?,
                    SF::new(len),
                    if included { "" } else { "will skip" }
                )
            }
            info!("--list was passed, nothing to do, exiting.");
            return Ok(None);
        }

        let output_folder = opts
            .output_folder
            .map(PathBuf::from)
            .unwrap_or_else(|| self.output_folder.clone());

        let mut builder = TorrentManagerBuilder::new(info, info_hash, output_folder);
        builder
            .overwrite(opts.overwrite)
            .spawner(self.spawner)
            .peer_id(self.peer_id);
        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }
        if let Some(interval) = opts.force_tracker_interval {
            builder.force_tracker_interval(interval);
        }

        if let Some(t) = opts.peer_opts.unwrap_or(self.peer_opts).connect_timeout {
            builder.peer_connect_timeout(t);
        }

        // let http_api = HttpApi::new(self.dht.clone());
        // spawn("HTTP API", {
        //     let http_api = http_api.clone();
        //     async move { http_api.make_http_api_and_run(http_api_listen_addr).await }
        // });

        let handle = builder.start_manager()?;
        if let Some(http_api) = self.http_api.as_ref() {
            http_api.add_mgr(handle.clone());
        }

        for url in trackers {
            handle.add_tracker(url);
        }
        for peer in initial_peers {
            handle.add_peer(peer);
        }

        if let Some(mut dht_peer_rx) = dht_peer_rx {
            spawn("DHT peer adder", {
                let handle = handle.clone();
                async move {
                    while let Some(peer) = dht_peer_rx.next().await {
                        handle.add_peer(peer);
                    }
                    warn!("dht was closed");
                    Ok(())
                }
            });
        }

        Ok(Some(handle))
    }
}