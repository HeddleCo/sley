use std::fs;
use std::path::Path;

pub fn set_mtime(path: impl AsRef<Path>, time: filetime::FileTime) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        options.access_mode(FILE_WRITE_ATTRIBUTES);
    }
    options.open(path)?.set_modified(time.into())
}
