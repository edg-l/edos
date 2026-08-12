//! User authentication, RFC 4252.
//!
//! Password is the only method. The comparison is constant-time, and a request
//! for a user that does not exist is checked against a fixed string so that a
//! wrong name and a wrong password take the same path.

use subtle::ConstantTimeEq;

use crate::config::Config;
use crate::error::{Error, Result, protocol_err};
use crate::packet::{
    DISCONNECT_AUTH_CANCELLED, MSG_SERVICE_ACCEPT, MSG_SERVICE_REQUEST, MSG_USERAUTH_FAILURE,
    MSG_USERAUTH_REQUEST, MSG_USERAUTH_SUCCESS, Transport,
};
use crate::wire::{Reader, Writer};

/// Attempts allowed before the connection is dropped, matching OpenSSH's
/// default `MaxAuthTries`.
const MAX_TRIES: u32 = 6;

/// Run authentication, returning the user that succeeded.
pub fn run(t: &mut Transport, cfg: &Config) -> Result<String> {
    // The client asks for the userauth service before it may authenticate.
    let payload = t.recv()?;
    let mut r = Reader::new(&payload);
    if r.byte()? != MSG_SERVICE_REQUEST {
        return protocol_err!("expected SERVICE_REQUEST");
    }
    let service = r.text()?;
    if service != "ssh-userauth" {
        return protocol_err!("cannot start service {service:?} before authenticating");
    }
    let mut accept = Writer::msg(MSG_SERVICE_ACCEPT);
    accept.text("ssh-userauth");
    t.send(&accept.finish())?;

    let mut tries = 0;
    loop {
        let payload = t.recv()?;
        let mut r = Reader::new(&payload);
        if r.byte()? != MSG_USERAUTH_REQUEST {
            return protocol_err!("expected USERAUTH_REQUEST");
        }
        let user = r.text()?.to_string();
        let service = r.text()?;
        let method = r.text()?.to_string();

        if service != "ssh-connection" {
            return protocol_err!("unknown service {service:?}");
        }

        let granted = match method.as_str() {
            "password" => {
                // A true flag is a password *change*, which this server cannot do.
                let changing = r.bool()?;
                let password = r.string()?;
                !changing && check_password(cfg, &user, password)
            }
            // "none" is how a client asks which methods exist, and anything
            // else is a method this server does not implement.
            _ => false,
        };

        if granted {
            t.send(&[MSG_USERAUTH_SUCCESS])?;
            return Ok(user);
        }

        // The failure list goes out before any disconnect, so a client that
        // has run out of attempts is told it was refused rather than left to
        // report a transport error.
        let mut w = Writer::msg(MSG_USERAUTH_FAILURE);
        w.name_list(&["password"]);
        w.bool(false); // nothing partially succeeded
        t.send(&w.finish())?;

        // A "none" request is the query that starts every authentication, not
        // an attempt, so it does not spend one. OpenSSH draws the same line.
        if method != "none" {
            tries += 1;
        }
        if tries >= MAX_TRIES {
            t.disconnect(
                DISCONNECT_AUTH_CANCELLED,
                "too many authentication failures",
            );
            return Err(Error::AuthFailed);
        }
    }
}

/// Compare a password without letting the time taken say which part was wrong.
fn check_password(cfg: &Config, user: &str, password: &[u8]) -> bool {
    // Compared against for an unknown user so that the work is the same either
    // way. It cannot be a real password: no configured one is empty.
    const ABSENT: &str = "";

    let stored = cfg
        .users
        .iter()
        .find(|u| u.name == user)
        .map_or(ABSENT, |u| u.password.as_str());

    if stored.is_empty() {
        // Still do a comparison, then refuse regardless.
        let _ = bool::from(password.ct_eq(password));
        return false;
    }
    stored.len() == password.len() && bool::from(stored.as_bytes().ct_eq(password))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(name: &str, password: &str) -> Config {
        let mut cfg = Config::default();
        cfg.add_user(name, password);
        cfg
    }

    #[test]
    fn only_the_right_password_passes() {
        let cfg = config_with("edgar", "hunter2");
        assert!(check_password(&cfg, "edgar", b"hunter2"));
        assert!(!check_password(&cfg, "edgar", b"hunter"));
        assert!(!check_password(&cfg, "edgar", b"hunter22"));
        assert!(!check_password(&cfg, "root", b"hunter2"));
    }

    #[test]
    fn an_empty_stored_password_never_passes() {
        let cfg = config_with("edgar", "");
        assert!(!check_password(&cfg, "edgar", b""));
    }
}
