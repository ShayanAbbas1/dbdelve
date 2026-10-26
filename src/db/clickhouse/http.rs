//! The HTTP side of ClickHouse: the server's errors, and the agent beside them.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the driver that sends requests arrives with Task 1.6"
    )
)]

use crate::db::DbError;

/// A server's `Code: N. DB::Exception: <message> (NAME) (version V)` as the
/// message alone, with the position it names checked against `statement`.
pub fn server_error(text: &str, statement: &str) -> DbError {
    let mut message = text.trim();
    if let Some(rest) = message.strip_prefix("Code: ")
        && let Some((digits, rest)) = rest.split_once(". DB::Exception: ")
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        message = rest;
    }
    if message.ends_with(')')
        && let Some(at) = message.rfind(" (version ")
    {
        message = message[..at].trim_end();
    }
    DbError {
        message: message.to_string(),
        position: position(message, statement),
    }
}

/// "failed at position N" counts bytes from 1, over the statement as sent.
fn position(message: &str, statement: &str) -> Option<usize> {
    let (_, rest) = message.split_once("failed at position ")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let at = rest[..digits].parse::<usize>().ok()?.checked_sub(1)?;
    (at < statement.len() && statement.is_char_boundary(at)).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTAX: &str = "Code: 62. DB::Exception: Syntax error: failed at position 13 ('FRM') (line 1, col 13): FRM t. Expected one of: token, Comma (SYNTAX_ERROR) (version 25.8.1.5101 (official build))\n";

    #[test]
    fn the_prefix_and_the_version_are_dropped_and_the_name_kept() {
        let error = server_error(SYNTAX, "SELECT 'é' FRM t");
        assert_eq!(
            error.message,
            "Syntax error: failed at position 13 ('FRM') (line 1, col 13): FRM t. Expected one of: token, Comma (SYNTAX_ERROR)"
        );
    }

    #[test]
    fn a_position_is_a_one_based_byte_offset_made_zero_based() {
        let statement = "SELECT 'é' FRM t";
        assert_eq!(&statement[12..15], "FRM");
        let error = server_error(SYNTAX, statement);
        assert_eq!(error.position, Some(12));
    }

    #[test]
    fn a_position_outside_the_statement_or_inside_a_character_is_dropped() {
        let statement = "SELECT 'é' FRM t";
        let at = |n: usize| {
            server_error(
                &format!("Code: 62. DB::Exception: failed at position {n} (X) (version 1)"),
                statement,
            )
            .position
        };
        assert_eq!(at(1), Some(0));
        assert_eq!(at(99), None);
        assert_eq!(at(statement.len() + 1), None);
        // Byte 9 is the second byte of the é.
        assert_eq!(at(10), None);
        assert_eq!(at(0), None);
    }
}
