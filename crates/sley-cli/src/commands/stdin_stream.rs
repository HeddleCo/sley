use std::io::{self, BufRead, Write};

use sley_core::Result;

pub(crate) struct StdinRecordReader<R> {
    reader: R,
    terminator: u8,
}

impl<R: BufRead> StdinRecordReader<R> {
    pub(crate) fn new(reader: R, terminator: u8) -> Self {
        Self { reader, terminator }
    }

    pub(crate) fn read_record(&mut self) -> Result<Option<Vec<u8>>> {
        let mut record = Vec::new();
        let read = self.reader.read_until(self.terminator, &mut record)?;
        if read == 0 {
            return Ok(None);
        }
        if record.last().copied() == Some(self.terminator) {
            record.pop();
        }
        Ok(Some(record))
    }
}

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
