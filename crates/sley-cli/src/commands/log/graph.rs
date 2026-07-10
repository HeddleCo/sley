use super::*;
use sley::plumbing::sley_rev;

pub(super) fn graph_show_commit(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut shown = false;
    while !shown && !graph.is_commit_finished() {
        let mut row = String::new();
        shown = graph.next_line(&mut row);
        out.write_all(row.as_bytes())?;
        if !shown {
            out.write_all(b"\n")?;
            write!(out, "{prefix}")?;
        }
    }
    Ok(())
}

/// Emit a single graph row (no trailing newline).
pub(super) fn graph_show_oneline(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut row = String::new();
    graph.next_line(&mut row);
    out.write_all(row.as_bytes())?;
    Ok(())
}

/// Emit a padding row (no trailing newline).
pub(super) fn graph_show_padding(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    let mut row = String::new();
    graph.padding_line(&mut row);
    out.write_all(row.as_bytes())?;
    Ok(())
}

/// Emit the remaining graph rows for the current commit; ends WITHOUT a
/// trailing newline (upstream `graph_show_remainder`).
fn graph_show_remainder(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write!(out, "{prefix}")?;
    if graph.is_commit_finished() {
        return Ok(());
    }
    loop {
        let mut row = String::new();
        graph.next_line(&mut row);
        out.write_all(row.as_bytes())?;
        if !graph.is_commit_finished() {
            out.write_all(b"\n")?;
            write!(out, "{prefix}")?;
        } else {
            break;
        }
    }
    Ok(())
}

/// Print `msg` line by line, with a graph row before every line but the first
/// (upstream `graph_show_strbuf`).
fn graph_show_strbuf(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    msg: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let mut start = 0usize;
    while start < msg.len() {
        let end = msg[start..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|pos| start + pos + 1)
            .unwrap_or(msg.len());
        out.write_all(&msg[start..end])?;
        let ended_with_newline = msg[end - 1] == b'\n';
        if ended_with_newline && end < msg.len() {
            graph_show_oneline(graph, prefix, out)?;
        }
        start = end;
    }
    Ok(())
}

/// Print the commit message followed by any remaining graph rows (upstream
/// `graph_show_commit_msg`).
pub(super) fn graph_show_commit_msg(
    graph: &mut sley_rev::graph::Graph,
    prefix: &str,
    msg: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    graph_show_strbuf(graph, prefix, msg, out)?;
    let newline_terminated = msg.last() == Some(&b'\n');
    if !graph.is_commit_finished() {
        if !newline_terminated {
            out.write_all(b"\n")?;
        }
        graph_show_remainder(graph, prefix, out)?;
        if newline_terminated {
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}
