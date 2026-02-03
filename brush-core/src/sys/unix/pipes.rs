pub type PipeReader = std::io::PipeReader;
pub type PipeWriter = std::io::PipeWriter;

pub fn pipe() -> std::io::Result<(PipeReader, PipeWriter)> {
    std::io::pipe()
}
