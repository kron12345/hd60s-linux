//! `~/.config/hd60s-linux/serve.conf`: `key = value` lines with the names
//! of the `serve` flags, turned into arguments. Flags given on the command
//! line win because they are searched first.

use std::path::PathBuf;

pub fn serve_config_arguments() -> Vec<String> {
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("hd60s-linux/serve.conf"));
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    parse(&text)
}

pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .flat_map(|(k, v)| {
            vec![
                format!("--{}", k.trim()),
                v.trim().trim_matches('"').to_string(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn config_lines_become_flags() {
        let flags =
            super::parse("# comment\npanel = on\nstream-fps=20\nrecord-dir = \"/srv/rec\"\n");
        assert_eq!(
            flags,
            [
                "--panel",
                "on",
                "--stream-fps",
                "20",
                "--record-dir",
                "/srv/rec"
            ]
        );
    }
}
