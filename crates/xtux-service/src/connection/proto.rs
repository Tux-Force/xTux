use crate::simple_context::SimpleContext;

pub async fn handle(
    _socket: &mut tokio::net::UnixStream,
    _context: &mut SimpleContext,
) -> tokio::io::Result<()> {
    // Reserved; protocol v2 uses its own magic (see xtux-protocol).
    Err(tokio::io::Error::other("the protobuf path is not implemented"))
}
