//! Process-wide state for an "admin window": a second Som process that was
//! launched elevated (UAC `runas`) because a tab profile in `settings.json`
//! sets `"admin": true`.
//!
//! An elevated process cannot be a child of this (non-elevated) process's
//! ConPTY, so such a profile can't open as an ordinary in-process tab — it
//! opens as its OWN Som window instead, launched with `--admin-profile
//! <name>`. That child process:
//!   * opens exactly one tab, with that profile, instead of restoring
//!     `db.json`'s session;
//!   * never writes `db.json` (see `Workspace::som_persist_db_json`), so it
//!     can't clobber the main window's saved tab layout.

use std::sync::OnceLock;

static ADMIN_PROFILE: OnceLock<String> = OnceLock::new();

/// Called once from `main` when `--admin-profile` was passed.
pub fn set_admin_profile(name: String) {
    if ADMIN_PROFILE.set(name).is_err() {
        log::warn!("som_admin::set_admin_profile called more than once; keeping the first value");
    }
}

/// `Some(profile name)` iff this process IS an admin-launched child window.
pub fn admin_profile() -> Option<&'static str> {
    ADMIN_PROFILE.get().map(String::as_str)
}

/// Whether this process is an admin-launched child window.
pub fn is_admin_session() -> bool {
    ADMIN_PROFILE.get().is_some()
}
