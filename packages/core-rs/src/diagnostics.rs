use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VrchatDiagnostics {
    pub checked_at_ms: i64,
    pub osc_enabled: Option<bool>,
    pub self_interact_enabled: Option<bool>,
    pub everyone_interact_enabled: Option<bool>,
    pub latest_log_path: Option<PathBuf>,
    pub osc_start_failure: Option<String>,
    pub oscquery_port_from_logs: Option<u16>,
    pub osc_launch_arg: Option<String>,
    pub osc_input_port: Option<u16>,
    pub osc_output_host: Option<String>,
    pub osc_output_port: Option<u16>,
    pub errors: Vec<String>,
}

impl VrchatDiagnostics {
    pub fn warning_lines(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.osc_enabled == Some(false) {
            warnings.push(
                "VRChat OSC appears disabled. Enable Options > OSC > Enabled in VRChat."
                    .to_string(),
            );
        }
        if self.self_interact_enabled == Some(false) {
            warnings.push(
                "VRChat Self Interact appears disabled. Enable Settings > Avatar Interactions > Self Interact."
                    .to_string(),
            );
        }
        if self.everyone_interact_enabled == Some(false) {
            warnings.push(
                "VRChat interaction level is not Everyone. Set Avatar Interactions to Everyone."
                    .to_string(),
            );
        }
        if let Some(failure) = &self.osc_start_failure {
            warnings.push(format!("VRChat log reports OSC startup failure: {failure}"));
        }
        warnings
    }
}

pub fn collect_vrchat_diagnostics(config_dir_override: Option<&Path>) -> VrchatDiagnostics {
    let mut out = VrchatDiagnostics {
        checked_at_ms: unix_ms_now(),
        ..VrchatDiagnostics::default()
    };

    match read_vrchat_registry_flags() {
        Ok((osc_enabled, self_interact_enabled, everyone_interact_enabled)) => {
            out.osc_enabled = osc_enabled;
            out.self_interact_enabled = self_interact_enabled;
            out.everyone_interact_enabled = everyone_interact_enabled;
        }
        Err(err) => out.errors.push(format!("Registry check failed: {err}")),
    }

    let vrchat_log_dir = config_dir_override
        .map(Path::to_path_buf)
        .or_else(default_vrchat_log_dir);
    let Some(vrchat_log_dir) = vrchat_log_dir else {
        out.errors
            .push("Could not resolve VRChat log directory".to_string());
        return out;
    };

    match find_latest_vrchat_log(&vrchat_log_dir) {
        Ok(Some(path)) => {
            out.latest_log_path = Some(path.clone());
            match scan_vrchat_log(&path) {
                Ok(scan) => {
                    out.osc_start_failure = scan.osc_start_failure;
                    out.oscquery_port_from_logs = scan.oscquery_port;
                    out.osc_launch_arg = scan.osc_launch_arg;
                    out.osc_input_port = scan.osc_input_port;
                    out.osc_output_host = scan.osc_output_host;
                    out.osc_output_port = scan.osc_output_port;
                }
                Err(err) => out
                    .errors
                    .push(format!("Failed scanning VRChat log: {err}")),
            }
        }
        Ok(None) => out.errors.push(format!(
            "No output_log file found in {}",
            vrchat_log_dir.display()
        )),
        Err(err) => out.errors.push(format!(
            "Failed to enumerate VRChat logs in {}: {err}",
            vrchat_log_dir.display()
        )),
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VrchatLogScan {
    pub osc_start_failure: Option<String>,
    pub oscquery_port: Option<u16>,
    pub osc_launch_arg: Option<String>,
    pub osc_input_port: Option<u16>,
    pub osc_output_host: Option<String>,
    pub osc_output_port: Option<u16>,
}

pub fn scan_vrchat_log(path: &Path) -> Result<VrchatLogScan, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    let start = file_len.saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;

    let slice = if start > 0 {
        if let Some(idx) = bytes.iter().position(|b| *b == b'\n') {
            &bytes[(idx + 1)..]
        } else {
            &bytes[..]
        }
    } else {
        &bytes[..]
    };
    let text = String::from_utf8_lossy(slice);
    let mut scan = VrchatLogScan::default();

    for line in text.lines() {
        if line.contains("Could not Start OSC") {
            scan.osc_start_failure = Some(line.trim().to_string());
        }
        if let Some(port) = extract_oscquery_port_from_log_line(&line) {
            scan.oscquery_port = Some(port);
        }
        if let Some(config) = extract_osc_launch_config_from_log_line(line) {
            scan.osc_launch_arg = Some(config.raw);
            scan.osc_input_port = Some(config.input_port);
            scan.osc_output_host = Some(config.output_host);
            scan.osc_output_port = Some(config.output_port);
        }
    }
    Ok(scan)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscLaunchConfig {
    pub raw: String,
    pub input_port: u16,
    pub output_host: String,
    pub output_port: u16,
}

pub fn extract_osc_launch_config_from_log_line(line: &str) -> Option<OscLaunchConfig> {
    let needle = "--osc=";
    let idx = line.find(needle)?;
    let rest = &line[(idx + needle.len())..];
    let token = rest
        .split_whitespace()
        .next()?
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    if token.is_empty() {
        return None;
    }

    let parts = token.split(':').collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    let input_port = parts.first()?.parse::<u16>().ok()?;
    let output_port = parts.last()?.parse::<u16>().ok()?;
    let output_host = parts[1..parts.len() - 1].join(":").trim().to_string();
    if output_host.is_empty() {
        return None;
    }

    Some(OscLaunchConfig {
        raw: token.to_string(),
        input_port,
        output_host,
        output_port,
    })
}

pub fn extract_oscquery_port_from_log_line(line: &str) -> Option<u16> {
    let needle = "of type OSCQuery on ";
    let idx = line.find(needle)?;
    let rest = &line[(idx + needle.len())..];
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let port = digits.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    Some(port)
}

pub fn default_vrchat_log_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")?;
        let mut path = PathBuf::from(appdata);
        path.pop();
        path.push("LocalLow");
        path.push("VRChat");
        path.push("VRChat");
        Some(path)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn find_latest_vrchat_log(dir: &Path) -> Result<Option<PathBuf>, String> {
    let read_dir = fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for entry_result in read_dir {
        let entry = entry_result.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("output_log") {
            continue;
        }
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        match &newest {
            Some((_, current)) if modified <= *current => {}
            _ => newest = Some((path, modified)),
        }
    }
    Ok(newest.map(|(path, _)| path))
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(windows)]
fn read_vrchat_registry_flags() -> Result<(Option<bool>, Option<bool>, Option<bool>), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    fn read_u32_value_casefold(key: &RegKey, target: &str) -> Result<Option<u32>, String> {
        let target_lower = target.to_ascii_lowercase();
        for item in key.enum_values() {
            let (name, _) = item.map_err(|e| e.to_string())?;
            let name_lower = name.to_ascii_lowercase();
            if name_lower == target_lower || name_lower.starts_with(&(target_lower.clone() + "_h"))
            {
                if let Ok(value) = key.get_value::<u32, _>(&name) {
                    return Ok(Some(value));
                }
            }
        }
        Ok(None)
    }

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey("Software\\VRChat\\VRChat")
        .map_err(|e| e.to_string())?;

    let osc = read_u32_value_casefold(&key, "UI.Settings.Osc")?;
    let self_interact = read_u32_value_casefold(&key, "VRC_AV_INTERACT_SELF")?;
    let everyone_interact = read_u32_value_casefold(&key, "VRC_AV_INTERACT_LEVEL")?;

    Ok((
        osc.map(|v| v == 1),
        self_interact.map(|v| v == 1),
        everyone_interact.map(|v| v == 2),
    ))
}

#[cfg(not(windows))]
fn read_vrchat_registry_flags() -> Result<(Option<bool>, Option<bool>, Option<bool>), String> {
    Ok((None, None, None))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn extracts_oscquery_port_from_log_line() {
        let line = "Some text [OSC] service of type OSCQuery on 37015 in process";
        assert_eq!(extract_oscquery_port_from_log_line(line), Some(37015));
        assert_eq!(extract_oscquery_port_from_log_line("no match"), None);
    }

    #[test]
    fn extracts_osc_launch_config_from_log_line() {
        let line = "Debug - Arg: --osc=6009:127.0.0.1:6010";
        let parsed = extract_osc_launch_config_from_log_line(line).expect("parsed launch arg");
        assert_eq!(parsed.input_port, 6009);
        assert_eq!(parsed.output_host, "127.0.0.1");
        assert_eq!(parsed.output_port, 6010);
        assert_eq!(parsed.raw, "6009:127.0.0.1:6010");
    }

    #[test]
    fn scans_vrchat_log_for_failure_and_port() {
        let tmp_file = std::env::temp_dir().join(format!(
            "vrl_diag_{}_{}.log",
            std::process::id(),
            unix_ms_now()
        ));
        let mut file = fs::File::create(&tmp_file).expect("create temp file");
        writeln!(file, "hello").expect("write");
        writeln!(file, "service of type OSCQuery on 39999").expect("write");
        writeln!(file, "Arg: --osc=6009:127.0.0.1:6010").expect("write");
        writeln!(file, "Could not Start OSC because socket in use").expect("write");
        drop(file);

        let scan = scan_vrchat_log(&tmp_file).expect("scan");
        assert_eq!(scan.oscquery_port, Some(39999));
        assert_eq!(scan.osc_input_port, Some(6009));
        assert_eq!(scan.osc_output_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(scan.osc_output_port, Some(6010));
        assert!(scan
            .osc_start_failure
            .as_deref()
            .is_some_and(|line| line.contains("Could not Start OSC")));

        let _ = fs::remove_file(tmp_file);
    }
}
