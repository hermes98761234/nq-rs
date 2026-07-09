//! Exec line writer — formats `exec ARG1 'ARG2'...` lines matching the original nq quoting rules.
//!
//! The quoting rules match the C `write_execline` function in nq.c:
//!
//! - If an argument contains no "unsafe" characters (control chars, space, and
//!   the shell-special set `` !"#$&'()*;<=>?[\]^`{|}~``, plus DEL), it is written
//!   as-is with a leading space separator.
//! - Otherwise, the argument is wrapped in single quotes (`'...'`), and any
//!   interior single quote is escaped as `'\''` (end-quote, literal quote, resume-quote).

use std::io::Write;

/// Check if a byte is "safe" (does not need quoting per the nq rules).
pub(crate) fn is_safe_byte(b: u8) -> bool {
    // Unsafe: control chars (0x01-0x1F), space (0x20), DEL (0x7F),
    // and the shell-special set.
    const UNSAFE: &[u8] = b" !\"#$&'()*;<=>?[\\]^`{|}~\x7f";
    b >= 0x21 && !UNSAFE.contains(&b)
}

/// Write an `exec` line to `writer` with arguments quoted according to the
/// original nq rules.
///
/// The output looks like: `exec arg1 'quoted arg2' arg3`
///
/// This line can be executed as a shell script to re-queue a job.
pub fn write_exec_line<W: Write>(mut writer: W, args: &[&str]) -> std::io::Result<()> {
    write!(writer, "exec")?;
    for arg in args {
        if arg.is_empty() || arg.bytes().any(|b| !is_safe_byte(b)) {
            // Needs quoting: wrap in single quotes, escape interior quotes.
            write!(writer, " '")?;
            for &b in arg.as_bytes() {
                if b == b'\'' {
                    write!(writer, "'\\''")?;
                } else {
                    writer.write_all(&[b])?;
                }
            }
            write!(writer, "'")?;
        } else {
            write!(writer, " {}", arg)?;
        }
    }
    writeln!(writer)?;
    Ok(())
}

/// Build the exec line string without writing to any output.
pub fn build_exec_line(args: &[&str]) -> String {
    let mut out = String::from("exec");
    for arg in args {
        if arg.is_empty() || arg.bytes().any(|b| !is_safe_byte(b)) {
            out.push_str(" '");
            for &b in arg.as_bytes() {
                if b == b'\'' {
                    out.push_str("'\\''");
                } else {
                    out.push(b as char);
                }
            }
            out.push('\'');
        } else {
            out.push(' ');
            out.push_str(arg);
        }
    }
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_args() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["make", "clean"]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec make clean\n");
    }

    #[test]
    fn test_arg_with_space() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "hello world"]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec echo 'hello world'\n");
    }

    #[test]
    fn test_arg_with_single_quote() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "it's fine"]).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "exec echo 'it'\\''s fine'\n"
        );
    }

    #[test]
    fn test_empty_arg() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", ""]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec echo ''\n");
    }

    #[test]
    fn test_special_chars_mixed() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "file$PATH", "normal"]).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "exec echo 'file$PATH' normal\n"
        );
    }

    #[test]
    fn test_multiple_quoted_args() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["wget", "http://example.com/file?q=a&b=c"]).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "exec wget 'http://example.com/file?q=a&b=c'\n"
        );
    }

    #[test]
    fn test_backslash_and_backtick() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "a\\b", "c`d"]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec echo 'a\\b' 'c`d'\n");
    }

    #[test]
    fn test_shell_roundtrip() {
        // The exec line should be valid shell syntax.
        // "exec echo normal 'quoted arg' 'it'\\''s'"
        // When parsed by sh, this should produce args: ["echo", "normal", "quoted arg", "it's"]
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "normal", "quoted arg", "it's"]).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(line, "exec echo normal 'quoted arg' 'it'\\''s'\n");
    }

    #[test]
    fn test_args_with_deep_paths() {
        // Common case: paths with slashes, dots are safe.
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["./configure", "--prefix=/usr/local"]).unwrap();
        // Slashes, dots, hyphens are safe, but `=` is unsafe.
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "exec ./configure '--prefix=/usr/local'\n"
        );
    }

    #[test]
    fn test_dollar_brace_in_arg() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "${HOME}"]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec echo '${HOME}'\n");
    }

    #[test]
    fn test_semicolon_in_arg() {
        let mut buf = Vec::new();
        write_exec_line(&mut buf, &["echo", "a;b"]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "exec echo 'a;b'\n");
    }
}
