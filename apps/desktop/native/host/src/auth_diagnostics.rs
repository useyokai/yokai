//! Bounded native evidence, separate from connection admission and renderer authority.
use serde::Serialize;
use yokai_desktop_contract::FailureKind;

pub const MAX_AUTH_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthTicket { pub sequence: u64, pub generation: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome { Started, Connected, Failed }

#[derive(Clone, Debug, Serialize)]
pub struct AuthRecord {
    pub sequence: u64,
    pub generation: u64,
    pub outcome: AuthOutcome,
    pub remote_entered: bool,
    pub remote_succeeded: bool,
    pub running_status_confirmed: bool,
    pub failure: Option<FailureKind>,
    pub retained_io: bool,
}

#[derive(Clone, Default, Serialize)]
pub struct AuthJournal {
    records: Vec<AuthRecord>,
    faulted: bool,
}

impl AuthJournal {
    pub fn records(&self) -> &[AuthRecord] { &self.records }
    pub fn faulted(&self) -> bool { self.faulted }

    fn refuse<T>(&mut self) -> Result<T, &'static str> {
        self.faulted = true;
        Err("Authentication evidence sequence refused.")
    }

    pub fn begin(&mut self, generation: u64) -> Result<AuthTicket, &'static str> {
        if self.faulted || generation == 0 || self.records.len() >= MAX_AUTH_ATTEMPTS
            || self.records.last().is_some_and(|r| r.outcome == AuthOutcome::Started
                || r.retained_io || generation <= r.generation) {
            return self.refuse();
        }
        // The fixed history cap makes this count monotonic without wrapping.
        let sequence = self.records.len() as u64 + 1;
        self.records.push(AuthRecord { sequence, generation, outcome: AuthOutcome::Started,
            remote_entered: false, remote_succeeded: false, running_status_confirmed: false, failure: None, retained_io: false });
        Ok(AuthTicket { sequence, generation })
    }

    fn pending(&mut self, ticket: AuthTicket) -> Result<&mut AuthRecord, &'static str> {
        if self.faulted || !self.records.last().is_some_and(|r| r.sequence == ticket.sequence
            && r.generation == ticket.generation && r.outcome == AuthOutcome::Started) {
            return self.refuse();
        }
        self.records.last_mut().ok_or("Authentication evidence is missing.")
    }

    pub fn remote_entered(&mut self, ticket: AuthTicket) -> Result<(), &'static str> {
        if self.pending(ticket)?.remote_entered { return self.refuse(); }
        self.pending(ticket)?.remote_entered = true;
        Ok(())
    }

    pub fn remote_succeeded(&mut self, ticket: AuthTicket) -> Result<(), &'static str> {
        let row = self.pending(ticket)?;
        if !row.remote_entered || row.remote_succeeded { return self.refuse(); }
        self.pending(ticket)?.remote_succeeded = true;
        Ok(())
    }

    pub fn status_confirmed(&mut self, ticket: AuthTicket) -> Result<(), &'static str> {
        let row = self.pending(ticket)?;
        if !row.remote_succeeded || row.running_status_confirmed { return self.refuse(); }
        self.pending(ticket)?.running_status_confirmed = true;
        Ok(())
    }

    pub fn finish(&mut self, ticket: AuthTicket, failure: Option<(FailureKind, bool)>) -> Result<(), &'static str> {
        let row = self.pending(ticket)?;
        if failure.is_none() && !(row.remote_succeeded && row.running_status_confirmed) {
            return self.refuse();
        }
        let row = self.pending(ticket)?;
        match failure {
            None => row.outcome = AuthOutcome::Connected,
            Some((kind, retained)) => {
                row.failure = Some(kind);
                row.retained_io = retained;
                row.outcome = AuthOutcome::Failed;
            }
        }
        Ok(())
    }

    pub fn require_complete(&self) -> Result<(), &'static str> {
        if self.faulted || self.records.iter().any(|r| r.outcome == AuthOutcome::Started || r.retained_io) {
            return Err("Authentication evidence incomplete or owner retained.");
        }
        Ok(())
    }
}
