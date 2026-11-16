//! Frame encoding and decoding.
//!
//! Wire layout: `u32 len (big-endian, of the rest) | u8 flags | u8 kind |
//! u16 stream | payload`. `len` is checked before any allocation so a corrupt
//! or hostile header cannot make the client reserve a huge buffer.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Payloads at least this large are zstd-compressed when it pays off. Small
/// messages gain nothing and would pay framing overhead for a few bytes.
pub const COMPRESS_THRESHOLD: usize = 4 * 1024;

/// Frames larger than this are refused before allocating. A blob read or a
/// snapshot chunk fits comfortably; an unbounded stream could exhaust memory.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The payload is zstd-compressed.
pub const FLAG_ZSTD: u8 = 1 << 0;
const KNOWN_FLAGS: u8 = FLAG_ZSTD;

pub const KIND_REQUEST: u8 = 1;
pub const KIND_RESPONSE: u8 = 2;
pub const KIND_EVENT: u8 = 3;

/// One frame plus the stream it belongs to.
///
/// The stream pairs a response with its request. Events use stream 0; their
/// correlation data (a diff ticket) lives in the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub stream: u16,
    pub message: Message,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte limit")]
    FrameTooLarge { len: usize },
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("unsupported frame flags {0:#04x}")]
    UnsupportedFlags(u8),
    #[error("compression failed: {0}")]
    Compress(String),
    #[error("decompression failed: {0}")]
    Decompress(String),
    #[error("message encoding failed: {0}")]
    Encode(String),
    #[error("message decoding failed: {0}")]
    Decode(String),
}

/// Writes one frame and flushes it, so a reader waiting behind a pipe sees it
/// without the writer buffering a whole conversation.
pub fn write_frame<W: Write>(
    writer: &mut W,
    stream: u16,
    message: &Message,
) -> Result<(), ProtocolError> {
    let payload = encode_message(message)?;
    let (flags, body) = if payload.len() >= COMPRESS_THRESHOLD {
        let compressed = zstd::bulk::compress(&payload, 3)
            .map_err(|error| ProtocolError::Compress(error.to_string()))?;
        // Incompressible content (already compressed blobs) would grow, so the
        // flag is only set when the result is actually smaller.
        if compressed.len() < payload.len() {
            (FLAG_ZSTD, compressed)
        } else {
            (0, payload)
        }
    } else {
        (0, payload)
    };

    let rest = 4 + body.len();
    if rest > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge { len: rest });
    }
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&(rest as u32).to_be_bytes());
    header[4] = flags;
    header[5] = message_kind(message);
    header[6..8].copy_from_slice(&stream.to_be_bytes());

    writer.write_all(&header)?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame. `Ok(None)` is a clean end of stream: the peer closed the
/// pipe between frames, which is how the helper is asked to shut down.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, ProtocolError> {
    let mut header = [0u8; 4];
    if read_first_byte(reader, &mut header[..1])? {
        // Read the rest of the length prefix.
    } else {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;

    // The length covers flags, kind, stream and payload; nothing shorter is a
    // frame this version can decode.
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge { len });
    }
    if len < 4 {
        return Err(ProtocolError::Malformed("frame length below header size"));
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;

    let flags = body[0];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(ProtocolError::UnsupportedFlags(flags));
    }
    let kind = body[1];
    let stream = u16::from_be_bytes([body[2], body[3]]);
    let payload = &body[4..];
    let payload = if flags & FLAG_ZSTD != 0 {
        let mut decoder = zstd::bulk::Decompressor::new()
            .map_err(|error| ProtocolError::Decompress(error.to_string()))?;
        decoder
            .decompress(payload, MAX_FRAME_BYTES)
            .map_err(|error| ProtocolError::Decompress(error.to_string()))?
    } else {
        payload.to_vec()
    };

    let message = decode_message(kind, &payload)?;
    Ok(Some(Frame { stream, message }))
}

/// Reads exactly one byte, tolerating signals that interrupt the read.
fn read_first_byte<R: Read>(reader: &mut R, byte: &mut [u8]) -> Result<bool, ProtocolError> {
    loop {
        match reader.read(byte) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn message_kind(message: &Message) -> u8 {
    match message {
        Message::Request(_) => KIND_REQUEST,
        Message::Response(_) => KIND_RESPONSE,
        Message::Event(_) => KIND_EVENT,
    }
}

fn decode_message(kind: u8, payload: &[u8]) -> Result<Message, ProtocolError> {
    match kind {
        KIND_REQUEST => Ok(Message::Request(decode_payload(payload)?)),
        KIND_RESPONSE => Ok(Message::Response(decode_payload(payload)?)),
        KIND_EVENT => Ok(Message::Event(decode_payload(payload)?)),
        other => Err(ProtocolError::UnknownKind(other)),
    }
}

fn decode_payload<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    postcard::from_bytes(payload).map_err(|error| ProtocolError::Decode(error.to_string()))
}

/// Encodes one message payload. The variant discriminator lives in the frame's
/// kind byte, so the payload holds only the inner request, response or event.
pub fn encode_message(message: &Message) -> Result<Vec<u8>, ProtocolError> {
    let encoded = match message {
        Message::Request(request) => postcard::to_allocvec(request),
        Message::Response(response) => postcard::to_allocvec(response),
        Message::Event(event) => postcard::to_allocvec(event),
    };
    encoded.map_err(|error| ProtocolError::Encode(error.to_string()))
}

/// Decodes a payload that [`encode_message`] produced.
pub fn decode_message_bytes(kind: u8, payload: &[u8]) -> Result<Message, ProtocolError> {
    decode_message(kind, payload)
}

/// Compile-time check that [`Message`] stays serializable by the helpers above.
fn _assert_serializable<T: Serialize + for<'de> Deserialize<'de>>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ErrorKind, Event, RemoteError, Request, Response};

    fn cursor(bytes: &[u8]) -> io::Cursor<&[u8]> {
        io::Cursor::new(bytes)
    }

    #[test]
    fn a_small_message_round_trips_without_compression() {
        let mut bytes = Vec::new();
        let message = Message::Request(Request::Ping { nonce: 7 });
        write_frame(&mut bytes, 3, &message).expect("write");
        assert_eq!(bytes[4] & FLAG_ZSTD, 0);
        assert_eq!(bytes[5], KIND_REQUEST);
        assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 3);

        let frame = read_frame(&mut cursor(&bytes))
            .expect("read")
            .expect("frame");
        assert_eq!(frame.stream, 3);
        assert_eq!(frame.message, message);
    }

    #[test]
    fn a_large_payload_is_compressed_and_round_trips() {
        let mut bytes = Vec::new();
        let message = Message::Response(Response::BlobAt {
            data: vec![b'a'; 16 * 1024],
        });
        write_frame(&mut bytes, 0, &message).expect("write");
        assert_ne!(bytes[4] & FLAG_ZSTD, 0);

        let frame = read_frame(&mut cursor(&bytes))
            .expect("read")
            .expect("frame");
        assert_eq!(frame.message, message);
    }

    #[test]
    fn an_incompressible_payload_stays_uncompressed() {
        let mut bytes = Vec::new();
        // A xorshift walk has no repeating byte pattern for zstd to exploit,
        // unlike a counter or product sequence.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..16 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        let message = Message::Response(Response::BlobAt { data });
        write_frame(&mut bytes, 0, &message).expect("write");
        assert_eq!(bytes[4] & FLAG_ZSTD, 0);

        let frame = read_frame(&mut cursor(&bytes))
            .expect("read")
            .expect("frame");
        assert_eq!(frame.message, message);
    }

    #[test]
    fn a_clean_eof_is_not_an_error() {
        let mut empty = cursor(&[]);
        assert_eq!(read_frame(&mut empty).expect("read"), None);
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let mut bytes = Vec::new();
        let message = Message::Request(Request::Ping { nonce: 1 });
        write_frame(&mut bytes, 0, &message).expect("write");
        bytes.truncate(bytes.len() - 1);

        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(matches!(error, ProtocolError::Io(_)), "{error:?}");
    }

    #[test]
    fn an_oversized_header_is_rejected_before_allocating() {
        // Only the header: a decoder that trusts it would try to reserve 64 MiB
        // and then block waiting for a body that never arrives.
        let mut bytes = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0, KIND_REQUEST, 0, 0]);
        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(
            matches!(error, ProtocolError::FrameTooLarge { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_short_length_is_rejected() {
        let bytes = 3u32.to_be_bytes();
        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(matches!(error, ProtocolError::Malformed(_)), "{error:?}");
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let payload = postcard::to_allocvec(&Response::Ok).expect("encode");
        let rest = 4 + payload.len();
        let mut bytes = (rest as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0, 42, 0, 0]);
        bytes.extend_from_slice(&payload);

        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(matches!(error, ProtocolError::UnknownKind(42)), "{error:?}");
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let payload = postcard::to_allocvec(&Response::Ok).expect("encode");
        let rest = 4 + payload.len();
        let mut bytes = (rest as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0x80, KIND_RESPONSE, 0, 0]);
        bytes.extend_from_slice(&payload);

        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(
            matches!(error, ProtocolError::UnsupportedFlags(0x80)),
            "{error:?}"
        );
    }

    #[test]
    fn a_corrupt_postcard_payload_is_a_clean_error() {
        let rest = 4 + 1;
        let mut bytes = (rest as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0, KIND_REQUEST, 0, 0, 0xff]);
        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(matches!(error, ProtocolError::Decode(_)), "{error:?}");
    }

    #[test]
    fn a_decompression_bomb_is_capped() {
        let payload = vec![0u8; MAX_FRAME_BYTES + 1];
        let compressed = zstd::bulk::compress(&payload, 1).expect("compress");
        let rest = 4 + compressed.len();
        let mut bytes = (rest as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[FLAG_ZSTD, KIND_RESPONSE, 0, 0]);
        bytes.extend_from_slice(&compressed);

        let error = read_frame(&mut cursor(&bytes)).expect_err("must fail");
        assert!(matches!(error, ProtocolError::Decompress(_)), "{error:?}");
    }

    #[test]
    fn the_error_message_wire_shape_round_trips() {
        let mut bytes = Vec::new();
        let message = Message::Response(Response::Error(RemoteError {
            kind: ErrorKind::Git,
            message: "bad revision".into(),
            stderr: Some("fatal: bad revision".into()),
            code: Some(128),
        }));
        write_frame(&mut bytes, 9, &message).expect("write");
        let frame = read_frame(&mut cursor(&bytes))
            .expect("read")
            .expect("frame");
        assert_eq!(frame.stream, 9);
        assert_eq!(frame.message, message);
    }

    #[test]
    fn every_request_and_response_round_trips() {
        use crate::dto::{DiffOptionsDto, DiffRequestDto, FileChangeDto, ObjectId, TreeEntryDto};

        let oid = ObjectId::from_bytes([3u8; 20]);
        let requests = vec![
            Request::Hello {
                protocol: crate::PROTOCOL_VERSION,
                app_version: "0.1.1".into(),
                build: "test".into(),
            },
            Request::Ping { nonce: 11 },
            Request::SubscribeSnapshots,
            Request::Reload,
            Request::Head,
            Request::CommitChanges { commit: oid },
            Request::TreeEntries {
                commit: oid,
                dir: "src".into(),
            },
            Request::BlobAt {
                commit: oid,
                path: "README.md".into(),
            },
            Request::CommitMessage { commit: oid },
            Request::Parents { commit: oid },
            Request::Diff {
                ticket: 7,
                request: DiffRequestDto::Staged { path: "a".into() },
                options: DiffOptionsDto {
                    context_lines: 3,
                    ignore_whitespace: false,
                },
            },
            Request::CancelDiff { ticket: 7 },
            Request::Run {
                id: 3,
                operation: crate::dto::OperationDto::Commit {
                    message: "subject".into(),
                    amend: false,
                },
            },
            Request::CancelOperation { id: 3 },
            Request::AnswerAskpass {
                id: 3,
                answer: Some("not-logged".into()),
            },
            Request::CommitsBetween {
                base: oid,
                tip: oid,
            },
            Request::IsAncestor {
                ancestor: oid,
                descendant: oid,
            },
            Request::MergeMessage,
            Request::Query {
                ticket: 4,
                query: crate::dto::QueryRequest::InProgressDetails,
            },
            Request::CancelQuery { ticket: 4 },
            Request::FileInWorktree {
                path: "conflict.txt".into(),
            },
            Request::ConflictSides {
                path: "conflict.txt".into(),
            },
            Request::ConflictStyle,
            Request::MergeBase { a: oid, b: oid },
            Request::ThreeWayTree {
                left: oid,
                base: oid,
                right: oid,
            },
            Request::ThreeWayBlobs {
                left: oid,
                base: oid,
                right: oid,
                path: "file.txt".into(),
            },
            Request::ThreeWayRoles {
                commits: [oid, oid, oid],
            },
            Request::ThreeWaySideTree {
                sides: [
                    crate::dto::RepoCompareSideDto::Commit(oid),
                    crate::dto::RepoCompareSideDto::Index,
                    crate::dto::RepoCompareSideDto::Worktree,
                ],
            },
            Request::CompareSideBlob {
                side: crate::dto::RepoCompareSideDto::Worktree,
                path: "file.txt".into(),
            },
            Request::CommitChangedFileCount { commit: oid },
            Request::LfsInfoAt {
                commit: oid,
                path: "data.bin".into(),
            },
            Request::LfsInfoInIndex {
                path: "data.bin".into(),
            },
            Request::LfsInfoInWorktree {
                path: "data.bin".into(),
            },
            Request::UsesLfs,
            Request::ReadLfsObject {
                oid: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
                size: 6,
                max_bytes: 1024,
            },
            Request::LfsAvailable,
            Request::LfsLocks,
            Request::FormatPatch { commit: oid },
            Request::GitConfig,
            Request::PreviewGitConfig {
                path: "/repo/.git/config".into(),
                layer: crate::dto::ConfigLayerDto::Local,
                key: "user.name".into(),
                change: crate::dto::ConfigChangeDto::Set("Ada".into()),
            },
        ];
        for request in requests {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, 4, &Message::Request(request.clone())).expect("write");
            let frame = read_frame(&mut cursor(&bytes))
                .expect("read")
                .expect("frame");
            assert_eq!(frame.stream, 4);
            assert_eq!(frame.message, Message::Request(request));
        }

        let responses = vec![
            Response::HelloAck {
                root: "/repo".into(),
                git_dir: "/repo/.git".into(),
                bare: false,
                watching: true,
            },
            Response::VersionMismatch {
                expected: 2,
                got: 1,
            },
            Response::Pong { nonce: 11 },
            Response::Head { head: None },
            Response::CommitChanges {
                changes: vec![FileChangeDto {
                    path: "a".into(),
                    old_path: None,
                    kind: crate::dto::ChangeKindDto::Modified,
                }],
            },
            Response::TreeEntries {
                entries: vec![TreeEntryDto {
                    name: "a".into(),
                    path: "a".into(),
                    kind: crate::dto::TreeEntryKindDto::File,
                    mode: 0o100_644,
                    size: Some(1),
                }],
            },
            Response::BlobAt {
                data: vec![1, 2, 3],
            },
            Response::CommitMessage {
                message: "subject".into(),
            },
            Response::Parents { parents: vec![oid] },
            Response::Ok,
            Response::Error(RemoteError::other("nope")),
            Response::CommitsBetween { commits: vec![oid] },
            Response::IsAncestor { yes: true },
            Response::MergeMessage {
                message: Some("Merge branch 'feature'".into()),
            },
            Response::FileInWorktree {
                data: Some(b"ours\n".to_vec()),
            },
            Response::ConflictSides {
                base: Some(b"base\n".to_vec()),
                ours: Some(b"ours\n".to_vec()),
                theirs: None,
            },
            Response::ConflictStyle {
                style: crate::dto::ConflictStyleDto::Diff3,
            },
            Response::MergeBase { commit: Some(oid) },
            Response::ThreeWayTree {
                tree: Box::new(crate::dto::ThreeWayTreeCompareDto {
                    left: crate::dto::RepoCompareSideDto::Commit(oid),
                    base: crate::dto::RepoCompareSideDto::Index,
                    right: crate::dto::RepoCompareSideDto::Worktree,
                    entries: Vec::new(),
                    counts: crate::dto::FolderCompareCountsDto {
                        total: 0,
                        identical: 0,
                        different: 0,
                        only_left: 0,
                        only_middle: 0,
                        only_right: 0,
                        type_mismatch: 0,
                    },
                    merge_tree_checked: false,
                }),
            },
            Response::ThreeWayBlobs {
                blobs: [Some(b"left\n".to_vec()), None, Some(b"right\n".to_vec())],
            },
            Response::ThreeWayRoles {
                left: oid,
                base: oid,
                right: oid,
            },
            Response::CompareSideBlob {
                data: Some(b"worktree\n".to_vec()),
            },
            Response::CommitChangedFileCount { count: 3 },
            Response::LfsInfo {
                info: Some(crate::dto::LfsInfoDto {
                    oid: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .into(),
                    size: 6,
                    present: true,
                }),
            },
            Response::UsesLfs { uses: true },
            Response::LfsObject {
                data: Some(b"hello\n".to_vec()),
            },
            Response::LfsAvailable { available: false },
            Response::LfsLocks {
                locks: vec![crate::dto::LfsLockDto {
                    id: "1".into(),
                    path: "data.bin".into(),
                    owner: Some("Ada".into()),
                    locked_at: None,
                }],
            },
            Response::FormatPatch {
                patch: b"From 1 Mon Sep 17 00:00:00 2001\n".to_vec(),
            },
            Response::GitConfig {
                config: Box::new(crate::dto::GitConfigDto {
                    entries: Vec::new(),
                    includes: Vec::new(),
                    identity: crate::dto::ConfigIdentityDto {
                        name: None,
                        email: None,
                    },
                    files: crate::dto::ConfigFilePathsDto {
                        local: "/repo/.git/config".into(),
                        worktree: None,
                        global: None,
                        system: None,
                    },
                }),
            },
            Response::ConfigPreview {
                command: "git config --file /repo/.git/config --replace-all user.name Ada".into(),
                before: String::new(),
                after: "[user]\n\tname = Ada\n".into(),
                len: 0,
                hash: 1,
                modified_secs: None,
                modified_nanos: None,
            },
        ];
        for response in responses {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, 2, &Message::Response(response.clone())).expect("write");
            let frame = read_frame(&mut cursor(&bytes))
                .expect("read")
                .expect("frame");
            assert_eq!(frame.stream, 2);
            assert_eq!(frame.message, Message::Response(response));
        }
    }

    #[test]
    fn events_round_trip() {
        let events = vec![
            Event::SnapshotAppend {
                generation: 3,
                commits: Vec::new(),
                rows: Vec::new(),
                complete: true,
            },
            Event::DiffFailed {
                ticket: 8,
                error: "not a commit".into(),
            },
            Event::LoadFailed {
                error: "deleted".into(),
            },
            Event::OperationStarted { id: 3 },
            Event::OperationProgress {
                id: 3,
                text: "Receiving objects: 45%".into(),
                percent: Some(45),
            },
            Event::OperationFinished { id: 3, error: None },
            Event::AskpassPrompt {
                id: 3,
                prompt: "Password: ".into(),
                secret: true,
            },
            Event::Query {
                ticket: 4,
                event: crate::dto::QueryEvent::PathHistoryDone,
            },
            Event::SignaturesInvalidated,
            Event::SnapshotMeta {
                snapshot: crate::dto::SnapshotDto {
                    generation: 4,
                    head: None,
                    refs: Vec::new(),
                    upstreams: Vec::new(),
                    stashes: Vec::new(),
                    remotes: Vec::new(),
                    status: crate::dto::WorktreeStatusDto::default(),
                    state: crate::dto::RepoStateDto::Normal,
                    conflicts: Vec::new(),
                    busy: None,
                },
            },
        ];
        for event in events {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, 0, &Message::Event(event.clone())).expect("write");
            let frame = read_frame(&mut cursor(&bytes))
                .expect("read")
                .expect("frame");
            assert_eq!(frame.stream, 0);
            assert_eq!(frame.message, Message::Event(event));
        }
    }

    #[test]
    fn serializable_helper_is_instantiated() {
        _assert_serializable::<Message>();
    }

    #[test]
    fn the_handshake_version_requires_a_current_helper() {
        // Deploy replaces a helper whose `--self-check` reports a different
        // protocol. Keeping the number here makes an accidental revert fail.
        assert_eq!(crate::PROTOCOL_VERSION, 7);
    }
}
