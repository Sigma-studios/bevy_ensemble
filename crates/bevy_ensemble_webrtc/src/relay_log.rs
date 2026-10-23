//! Every relay report, and every run's log, kept in a file a player can find and send in.
//!
//! A [`RelayReport`](crate::RelayReport) is logged as a warning, which is enough on the web — the
//! browser console keeps it — but a native build has no console anybody sees. So native builds
//! also append each report to [`relay_log_path`], one block per relayed connection. The host's
//! file is the useful one: every client's connection goes through the host, so its reports cover
//! everybody.

use bevy::prelude::*;

use crate::{RelayReport, SignallingDisplayName, WebrtcRuntime};

/// Where the reports go: the platform's usual place for an application's logs, in a folder named
/// after the game — its [`game`](crate::BevyEnsembleWebrtcPlugin::game) id, or the executable's
/// name when it declared none.
///
/// - macOS: `~/Library/Logs/<game>/relay-reports.txt`
/// - Windows: `%LOCALAPPDATA%\<game>\relay-reports.txt`
/// - Linux: `$XDG_STATE_HOME/<game>/relay-reports.txt`, or `~/.local/state/<game>/…`
pub fn relay_log_path(game: &str) -> Option<std::path::PathBuf> {
    use std::env::var_os;
    use std::path::PathBuf;

    let folder = if game.is_empty() {
        std::env::current_exe()
            .ok()?
            .file_stem()?
            .to_string_lossy()
            .into_owned()
    } else {
        game.to_owned()
    };
    let dir = if cfg!(target_os = "macos") {
        PathBuf::from(var_os("HOME")?).join("Library/Logs")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(var_os("LOCALAPPDATA")?)
    } else {
        var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?
    };
    Some(dir.join(folder).join("relay-reports.txt"))
}

/// Where this run's log goes: `session.log`, beside [`relay_log_path`]'s reports. The run before
/// is kept as `session.previous.log`.
pub fn session_log_path(game: &str) -> Option<std::path::PathBuf> {
    Some(relay_log_path(game)?.with_file_name("session.log"))
}

/// Every line the console gets, in a file a player can find and send in: for
/// `LogPlugin::custom_layer`, as `custom_layer: |_| session_log_layer("my-game")`.
///
/// A native build's console is a terminal nobody launched it from, so what a peer saw when a
/// session went wrong was lost the moment it happened — and the peer that saw the most is often
/// the one that is not the developer's: a host on somebody else's machine. The file is the run's,
/// started afresh each launch; the run before is moved to `session.previous.log`, so a player who
/// started the game again after a bad session to look for the file has not just overwritten it.
/// What reaches it is what the `LogPlugin`'s filter lets through, as for the console.
///
/// `None` where there is nowhere to write: no home directory, a folder that cannot be made. On
/// the web there is no file, and the browser console keeps everything already.
pub fn session_log_layer(game: &str) -> Option<bevy::log::BoxedLayer> {
    use bevy::log::tracing_subscriber::{Layer, fmt};

    let path = session_log_path(game)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let _ = std::fs::rename(&path, path.with_file_name("session.previous.log"));
    let file = std::fs::File::create(&path).ok()?;
    Some(
        fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .boxed(),
    )
}

pub(crate) fn keep_relay_reports(
    mut reports: MessageReader<RelayReport>,
    runtime: Res<WebrtcRuntime>,
    name: Option<Res<SignallingDisplayName>>,
) {
    for report in reports.read() {
        // Whose file it is: the name this peer is listed under, which is what the person reading
        // it will know them by. The other side is named by uuid, which is all this crate has.
        let who = name.as_ref().map_or("", |name| name.0.as_str());
        append(&runtime.game, who, report.peer, &report.report);
    }
}

fn append(game: &str, who: &str, peer: u128, report: &str) {
    use std::io::Write;

    let Some(path) = relay_log_path(game) else {
        warn!("nowhere to keep the relay report: no home directory");
        return;
    };
    let written = (|| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let when = utc_timestamp(seconds);
        writeln!(file, "=== {when}, \"{who}\" to peer {peer:#x}\n{report}").ok()
    })();
    match written {
        Some(()) => warn!("relay report kept in {}", path.display()),
        None => warn!("could not write the relay report to {}", path.display()),
    }
}

/// `seconds` since the Unix epoch as `2026-10-07 14:32:05 UTC`.
///
/// UTC rather than local time: the file gets sent in by players in other time zones, and a time
/// with its zone spelled out is one nobody has to guess at. Days to a calendar date is Howard
/// Hinnant's `civil_from_days`, which is all a date crate would be here for.
fn utc_timestamp(seconds: u64) -> String {
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    let (hour, minute, second) = (of_day / 3600, of_day / 60 % 60, of_day % 60);

    // Count from 0000-03-01, so a leap day falls at the end of its year.
    let days = days + 719_468;
    let era = days / 146_097;
    let of_era = days % 146_097;
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::utc_timestamp;

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_timestamp(1_790_803_633), "2026-09-30 21:27:13 UTC");
        assert_eq!(utc_timestamp(4_107_542_399), "2100-02-28 23:59:59 UTC");
    }
}
