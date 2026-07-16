/// A per-process, in-memory identifier used to scope KVP-style persistence
/// keys (e.g. dock panel sizes) to "this run of Som". Not persisted across
/// launches — Som has no "restore windows from last session" feature (it
/// always opens exactly one window, restoring tabs from `db.json` instead),
/// so there is nothing to read an old session id back for.
pub struct Session {
    session_id: String,
}

impl Session {
    pub fn new(session_id: String) -> Self {
        Self { session_id }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }
}

pub struct AppSession {
    session: Session,
}

impl AppSession {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    pub fn id(&self) -> &str {
        self.session.id()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_session_for_test(&mut self, session: Session) {
        self.session = session;
    }
}
