//! `/proc/net/tcp` parsing for `WaitForPort`. Pure functions, tested on any OS.

/// True if some socket in `tcp_table` (contents of /proc/net/tcp or tcp6)
/// is LISTENing on `port`.
pub fn listening_on(tcp_table: &str, port: u16) -> bool {
    let want = format!("{port:04X}");
    tcp_table.lines().skip(1).any(|line| {
        let mut cols = line.split_whitespace();
        let (Some(_sl), Some(local), Some(_rem), Some(state)) =
            (cols.next(), cols.next(), cols.next(), cols.next())
        else {
            return false;
        };
        state == "0A"
            && local
                .rsplit(':')
                .next()
                .map(|p| p.eq_ignore_ascii_case(&want))
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0
   1: 0100007F:0016 0100007F:C2A4 01 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000000000000000 20 4 30 10 -1
";

    #[test]
    fn finds_listening_port() {
        assert!(listening_on(TABLE, 8080));
        assert!(!listening_on(TABLE, 22), "established, not listening");
        assert!(!listening_on(TABLE, 9000));
        assert!(!listening_on("", 1));
    }
}
