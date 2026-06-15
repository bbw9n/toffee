//! Multiline accumulator for line-oriented sources where one record spans
//! multiple lines (`stdin` from `tmux pipe-pane`, aider history, etc.).
//!
//! Pattern borrowed from Vector / fluentd: a regex says "a new record
//! starts here." Until the next match, lines are appended to the current
//! buffer. When the next match arrives, the buffer is flushed and the new
//! line begins a fresh buffer.
//!
//! JSONL sources don't need this — every line is its own record. So the
//! Claude / Codex sources don't use this; `stdin` and `aider` will.

#[derive(Debug, Clone)]
pub struct MultilineConfig {
    /// Regex (compiled-to-substring for simplicity here) that marks the
    /// start of a new record. Each successive matching line flushes the
    /// buffer.
    ///
    /// For Phase 1 we accept a simple "starts-with" prefix instead of a
    /// regex to keep dependencies down. Real regex can land later.
    pub starts_with: Option<String>,
    /// Maximum buffer size before forced flush. Guards against runaway
    /// records on a misconfigured boundary.
    pub max_bytes: usize,
}

impl Default for MultilineConfig {
    fn default() -> Self {
        MultilineConfig {
            starts_with: None,
            max_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct MultilineAccumulator {
    config: MultilineConfig,
    buffer: String,
}

impl MultilineAccumulator {
    pub fn new(config: MultilineConfig) -> Self {
        MultilineAccumulator {
            config,
            buffer: String::new(),
        }
    }

    /// Feed one line. If the line begins a new record (or the buffer is
    /// over capacity), returns the previous record. The current line
    /// becomes the new buffer.
    pub fn push_line(&mut self, line: &str) -> Option<String> {
        let is_boundary = match &self.config.starts_with {
            Some(prefix) if !prefix.is_empty() => line.starts_with(prefix.as_str()),
            _ => true, // every line is its own record
        };
        if is_boundary && !self.buffer.is_empty() {
            let out = std::mem::take(&mut self.buffer);
            self.buffer.push_str(line);
            return Some(out);
        }
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);
        if self.buffer.len() >= self.config.max_bytes {
            return Some(std::mem::take(&mut self.buffer));
        }
        None
    }

    /// Flush whatever is left in the buffer.
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_boundary_yields_one_per_line() {
        let mut acc = MultilineAccumulator::new(MultilineConfig::default());
        let mut out = Vec::new();
        if let Some(r) = acc.push_line("alpha") {
            out.push(r);
        }
        if let Some(r) = acc.push_line("beta") {
            out.push(r);
        }
        if let Some(r) = acc.flush() {
            out.push(r);
        }
        assert_eq!(out, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn starts_with_groups_until_next_match() {
        let mut acc = MultilineAccumulator::new(MultilineConfig {
            starts_with: Some(">>>".into()),
            ..Default::default()
        });
        let mut out = Vec::new();
        let lines = [
            ">>> turn 1 line 1",
            "continued 1",
            "still 1",
            ">>> turn 2 line 1",
            "continued 2",
        ];
        for l in lines {
            if let Some(r) = acc.push_line(l) {
                out.push(r);
            }
        }
        if let Some(r) = acc.flush() {
            out.push(r);
        }
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with(">>> turn 1"));
        assert!(out[1].starts_with(">>> turn 2"));
        assert!(out[0].contains("still 1"));
    }
}
