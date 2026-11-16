//! Framed protocol between the app and a headless `strata-remote` helper.
//!
//! Frames travel over a byte stream (a pipe today, SSH stdio later). They
//! carry postcard-encoded messages whose payload eventually becomes the same
//! `SessionEvent`s a local session produces, so the UI can render a remote
//! repository without knowing where it lives.
//!
//! Types in `strata-core` are not serialized directly: their public shape is
//! shared with the UI, and deriving serde on them would make every field part
//! of the wire format. [`dto`] mirrors them instead and converts at the edge.

pub mod codec;
pub mod dto;
pub mod message;

pub use codec::{
    COMPRESS_THRESHOLD, FLAG_ZSTD, Frame, KIND_EVENT, KIND_REQUEST, KIND_RESPONSE, MAX_FRAME_BYTES,
    ProtocolError, read_frame, write_frame,
};
pub use dto::*;
pub use message::{ErrorKind, Event, Message, RemoteError, Request, Response};

/// Wire revision. The helper and the app must agree exactly; version
/// negotiation is a hard failure instead of a best-effort decode.
///
/// Version 2 adds mutations (`Run`), their progress events, credential
/// prompts and the reads a rebase confirmation needs. An older helper fails
/// the handshake, which is what makes the app replace it.
///
/// Version 3 adds detached checkout (`Operation::CheckoutCommit`). The
/// existing checkout keeps a trailing `--`, which would turn an object id
/// into a pathspec, so the new operation is its own request.
///
/// Version 4 adds ticketed queries (blame, history, search, compare, reflog,
/// signatures and the rest of the session's background reads). A cancel and a
/// dropped helper use the same ticket route as diffs: late results are
/// ignored, and an outstanding ticket expires with an error.
///
/// Version 5 adds the reads a conflict editor, a three-way compare, and the
/// per-commit changed-file count need. Conflict reads are the index stages,
/// the worktree bytes and `merge.conflictStyle`; the writes were already
/// operations. Three-way reads are the merge base, the roles of three commits,
/// and the tree and blob contents of commits, the index and the worktree. The
/// changed-file count walks trees against the first parent and stays separate
/// from the ticketed commit stat, so scrolling history does not inflate blobs.
/// An older helper fails the handshake, which is what makes the app replace it.
///
/// Version 6 tells the app whether the helper is watching the worktree
/// ([`message::Response::HelloAck`]) and carries worktree updates that do not
/// replace the commit list ([`message::Event::SnapshotMeta`]). A helper that
/// cannot install a watch still serves the repository; the app refreshes it
/// explicitly. An older helper fails the handshake, which is what makes the
/// app replace it.
///
/// Version 7 carries writes a remote tab runs through the same `Run` path as
/// a local one: reflog undo and redo, worktrees, submodule update, LFS track,
/// lock, unlock and pull, `git config`, and applying a patch's bytes. LFS
/// reads name an object id; the helper builds the object path itself. A
/// configuration write names a file the helper already listed, and the helper
/// runs `git config` rather than editing that file itself. Saving a commit as
/// a patch asks the helper for the bytes and writes the file here. Tags,
/// remotes, an external merge tool and the history scope stay off the wire.
/// Paths in these messages are checked on the helper before git sees them. An
/// older helper fails the handshake, which is what makes the app replace it.
pub const PROTOCOL_VERSION: u32 = 7;
