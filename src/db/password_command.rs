//! A login's password printed by a command the user gives, as pgAdmin's
//! "password exec command" does: how a cloud IAM token, which lasts minutes,
//! reaches a profile that outlives it.

/// A command whose standard output is the password, `%HOSTNAME%`, `%PORT%`
/// and `%USERNAME%` standing for the profile's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordCommand {
    pub command: String,
    /// Seconds its output is reused for; 0 runs it for every login.
    pub expires: u32,
}
