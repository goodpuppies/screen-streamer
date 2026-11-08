use std::io::{self, BufRead};

/// Deliver complete control lines on the stdin reader thread. In particular,
/// never make input wait for a captured frame or a blocked frame socket write.
pub fn forward_control_lines(
  mut reader: impl BufRead,
  mut send: impl FnMut(String) -> bool,
) -> io::Result<()> {
  loop {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
      return Ok(());
    }
    if !send(line) {
      return Ok(());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  #[test]
  fn preserves_command_order_and_stops_when_receiver_closes() {
    let mut commands = Vec::new();
    forward_control_lines(Cursor::new("K,11,1\nK,11,0\nM,0.5,0.5\n"), |line| {
      commands.push(line);
      commands.len() < 2
    })
    .unwrap();
    assert_eq!(commands, ["K,11,1\n", "K,11,0\n"]);
  }

  #[cfg(unix)]
  #[test]
  fn delivers_input_while_reader_remains_open_without_a_capture_tick() {
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    let (reader, mut writer) = UnixStream::pair().unwrap();
    let (tx, rx) = mpsc::channel();
    let input_thread = std::thread::spawn(move || {
      forward_control_lines(BufReader::new(reader), |line| tx.send(line).is_ok()).unwrap();
    });
    writer.write_all(b"K,11,1\nK,11,0\n").unwrap();
    // The stream is still open, and there is no running capture consumer to
    // drain a second queue. Both transitions must reach input nevertheless.
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "K,11,1\n");
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "K,11,0\n");
    drop(writer);
    input_thread.join().unwrap();
  }
}
