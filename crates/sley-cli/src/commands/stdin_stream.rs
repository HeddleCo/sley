use std::io::{self, Write};

use sley::plumbing::sley_core::{RecordReader, Result};

pub(crate) type StdinRecordReader<R> = RecordReader<R>;

pub(crate) fn stream_stdin_records<W, F>(
    terminator: u8,
    stdout: &mut W,
    mut process: F,
) -> Result<()>
where
    W: Write,
    F: FnMut(Vec<u8>, &mut W) -> Result<()>,
{
    let stdin = io::stdin();
    let mut reader = StdinRecordReader::new(stdin.lock(), terminator);
    while let Some(record) = reader.read_record()? {
        process(record, stdout)?;
        stdout.flush()?;
    }
    Ok(())
}

pub(crate) fn strip_trailing_cr(record: &mut Vec<u8>) {
    if record.last().copied() == Some(b'\r') {
        record.pop();
    }
}
