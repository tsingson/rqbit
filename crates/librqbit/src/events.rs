use librqbit_core::Id20;

#[derive(Clone, Copy, Debug)]
pub struct TorrentEvent {
    pub info_hash: Id20,
    pub kind: TorrentEventKind,
}

#[derive(Clone, Copy, Debug)]
pub enum TorrentEventKind {
    Added,
    Paused,
    Started,
    Deleted,
    Errored,
    Completed,
}
