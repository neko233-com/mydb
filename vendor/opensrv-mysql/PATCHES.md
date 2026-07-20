# Local protocol patch

This directory vendors `opensrv-mysql` 0.7.0 from the Apache-2.0 licensed
[`datafuselabs/opensrv`](https://github.com/datafuselabs/opensrv) project.

MyDB changes `AsyncMysqlShim::on_query` from `&str` to `&[u8]`. MySQL
`COM_QUERY` packets may contain raw bytes in `_binary` literals emitted by
`mysqldump`; forcing UTF-8 decoding disconnects valid clients before the query
reaches MyDB. Prepared-statement SQL remains UTF-8 because binary parameter
values use the protocol's separate parameter payload.

MyDB also makes `Packet` own its payload. Upstream returned a packet borrowing
the reader buffer and then replaced that buffer before returning whenever more
than one packet had already arrived. Concurrent prepared reads reproduced this
as Windows access violation `0xC0000005`; owned packets remove the use-after-free
and all unsafe code from `packet_reader.rs`.

MyDB advertises `CLIENT_LOCAL_FILES` and extends `AsyncMysqlShim` with an
explicit `LOAD DATA LOCAL INFILE` transfer hook. The intermediary emits the
standard `0xfb + file_name` request, receives the client's sequenced file
packets through the existing packet reader, drains oversize transfers safely,
and returns the final response at the correct sequence number. Backends control
whether a query is a local-file request and set a per-statement byte limit.

OK packets serialize `OkResponse.warnings`; upstream emitted zero regardless
of backend diagnostics. This lets LOAD DATA warning counts reach MySQL clients.

The error-code table includes MySQL 8 `ER_LOCK_NOWAIT` (3572), which is newer
than the upstream generated table and is required for locking-read compatibility.
