//! Reads a Git server's ref advertisement over SSH, in process: the controller image has no `ssh`.
//!
//! The server must present a host key that the Secret's `known_hosts` lists for it; there is no
//! trust on first use. The client runs `git-upload-pack`, reads the version 0 advertisement up to
//! its flush, answers with a flush of its own (it wants nothing) and disconnects. Ed25519 and
//! ECDSA keys only, for the host and the client: RSA would need the `rsa` crate.

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, Preferred};
use zeabur_builder_core::git;
use zeabur_builder_core::ssh::{SshRemote, known_keys};

use crate::remote::ResolveError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// For everything after the connection: key exchange, authentication and the advertisement.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The most of the server's stderr kept for the error message.
const MAX_STDERR: usize = 1024;

/// Accepts only the keys `known_hosts` lists.
struct KnownHosts {
    keys: Vec<PublicKey>,
}

impl client::Handler for KnownHosts {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        let known = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                let known = self.keys.iter().any(|k| k.key_data() == key.key_data());
                if !known {
                    let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
                    tracing::info!(%fingerprint, "SSH host key not in known_hosts");
                }
                known
            }
            PublicKeyOrCertificate::Certificate(_) => false,
        };
        std::future::ready(Ok(known))
    }
}

/// The trusted host keys for `remote` and the algorithms to ask the server for, in file order.
fn trusted(
    remote: &SshRemote,
    known_hosts: &str,
) -> Result<(Vec<PublicKey>, Vec<Algorithm>), ResolveError> {
    let mut keys = Vec::new();
    let mut algorithms = Vec::new();
    for known in known_keys(known_hosts, &remote.host, remote.port) {
        let Ok(algorithm) = Algorithm::from_str(known.algorithm) else {
            continue;
        };
        let Ok(key) = russh::keys::parse_public_key_base64(known.key) else {
            continue;
        };
        if key.algorithm() != algorithm || algorithm.clone().is_rsa() {
            continue;
        }
        if !algorithms.contains(&algorithm) {
            algorithms.push(algorithm);
        }
        keys.push(key);
    }
    if keys.is_empty() {
        return Err(ResolveError::Credentials(format!(
            "known_hosts has no Ed25519 or ECDSA key for {}",
            host_label(remote)
        )));
    }
    Ok((keys, algorithms))
}

fn host_label(remote: &SshRemote) -> String {
    if remote.port == 22 {
        remote.host.clone()
    } else {
        format!("[{}]:{}", remote.host, remote.port)
    }
}

/// The version 0 ref advertisement of the repository at `remote`.
pub(crate) async fn advertisement(
    remote: &SshRemote,
    private_key: &str,
    known_hosts: &str,
) -> Result<Vec<u8>, ResolveError> {
    let label = host_label(remote);
    let transport =
        |e: &dyn std::fmt::Display| ResolveError::Transport(label.clone(), e.to_string());
    let (keys, algorithms) = trusted(remote, known_hosts)?;
    let key = russh::keys::decode_secret_key(private_key, None).map_err(|e| {
        ResolveError::Credentials(format!(
            "ssh-privatekey is not an unencrypted OpenSSH key: {e}"
        ))
    })?;
    let config = Arc::new(client::Config {
        preferred: Preferred {
            key: algorithms.into(),
            ..Preferred::default()
        },
        inactivity_timeout: Some(READ_TIMEOUT),
        ..client::Config::default()
    });

    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((remote.host.as_str(), remote.port)),
    )
    .await
    .map_err(|e| transport(&e))?
    .map_err(|e| transport(&e))?;
    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    let handler = KnownHosts { keys };
    let mut session: Handle<KnownHosts> =
        match tokio::time::timeout_at(deadline, client::connect_stream(config, stream, handler))
            .await
            .map_err(|e| transport(&e))?
        {
            Ok(session) => session,
            Err(russh::Error::UnknownKey) => {
                return Err(ResolveError::HostKey(format!(
                    "{label} presented a host key that known_hosts does not list"
                )));
            }
            Err(e) => return Err(transport(&e)),
        };

    let auth = tokio::time::timeout_at(
        deadline,
        session.authenticate_publickey(
            remote.user.clone(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        ),
    )
    .await
    .map_err(|e| transport(&e))?
    .map_err(|e| transport(&e))?;
    if let AuthResult::Failure { .. } = auth {
        return Err(ResolveError::Denied(format!(
            "{label} refused the key for `{}`",
            remote.user
        )));
    }

    let result = tokio::time::timeout_at(deadline, read_advertisement(&session, remote))
        .await
        .map_err(|e| transport(&e))
        .and_then(|r| r);
    let quit = session.disconnect(Disconnect::ByApplication, "", "en");
    if let Err(e) = tokio::time::timeout(Duration::from_secs(1), quit).await {
        tracing::debug!(host = %label, error = %e, "SSH disconnect timed out");
    }
    result
}

async fn read_advertisement(
    session: &Handle<KnownHosts>,
    remote: &SshRemote,
) -> Result<Vec<u8>, ResolveError> {
    let transport =
        |e: &dyn std::fmt::Display| ResolveError::Transport(host_label(remote), e.to_string());
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| transport(&e))?;
    channel
        .exec(true, remote.upload_pack_command())
        .await
        .map_err(|e| transport(&e))?;
    let mut out = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    while let Some(message) = channel.wait().await {
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "the rest are requests a server never sends a client, or replies to them"
        )]
        match message {
            ChannelMsg::Data { data } => {
                out.extend_from_slice(&data);
                if let Some(end) = git::advertisement_end(&out)? {
                    out.truncate(end);
                    // A flush instead of `want` lines: the server ends without sending a pack.
                    channel
                        .data_bytes(&b"0000"[..])
                        .await
                        .map_err(|e| transport(&e))?;
                    if let Err(e) = channel.eof().await {
                        tracing::debug!(error = %e, "SSH channel already closed");
                    }
                    return Ok(out);
                }
            }
            ChannelMsg::ExtendedData { data, .. } => {
                let room = MAX_STDERR.saturating_sub(stderr.len());
                stderr.extend(data.iter().take(room));
            }
            ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
            ChannelMsg::Failure => {
                return Err(ResolveError::Denied(format!(
                    "{} refused to run git-upload-pack",
                    host_label(remote)
                )));
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
    if !stderr.is_empty() || exit.is_some_and(|s| s != 0) {
        return Err(ResolveError::Denied(format!(
            "{} answered: {}",
            host_label(remote),
            if stderr.is_empty() {
                format!("exit status {}", exit.unwrap_or_default())
            } else {
                stderr
            }
        )));
    }
    Err(transport(
        &"the connection closed before the ref list ended",
    ))
}

#[cfg(test)]
mod tests {
    use russh::client::Handler as _;

    use super::*;

    const GITHUB: &str = "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn remote(url: &str) -> SshRemote {
        SshRemote::parse(url).unwrap_or_else(|| SshRemote {
            user: String::new(),
            host: String::new(),
            port: 0,
            path: String::new(),
        })
    }

    #[test]
    fn trusts_listed_keys_only() {
        let github = remote("git@github.com:acme/api.git");
        let Ok((_, algorithms)) = trusted(&github, GITHUB) else {
            panic!("no key for github.com");
        };
        assert_eq!(algorithms, vec![Algorithm::Ed25519]);

        let other_port = remote("ssh://git@github.com:2222/acme/api.git");
        assert!(matches!(
            trusted(&other_port, GITHUB),
            Err(ResolveError::Credentials(_))
        ));
        let bracketed = format!(
            "[github.com]:2222 {}",
            GITHUB.trim_start_matches("github.com ")
        );
        assert!(trusted(&other_port, &bracketed).is_ok());
    }

    #[tokio::test]
    async fn rejects_an_unlisted_host_key() {
        let Ok((keys, _)) = trusted(&remote("git@github.com:acme/api.git"), GITHUB) else {
            panic!("no key for github.com");
        };
        let mut handler = KnownHosts { keys };
        let listed = russh::keys::parse_public_key_base64(
            "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
        );
        let other = russh::keys::parse_public_key_base64(
            "AAAAC3NzaC1lZDI1NTE5AAAAIAfHBXpPjGTLs+AGh3ZOFqv1BOmcuWyzAX2v3m7+CRdg",
        );
        let (Ok(listed), Ok(other)) = (listed, other) else {
            panic!("test keys do not parse");
        };
        let ask = |key: PublicKey| PublicKeyOrCertificate::PublicKey {
            key,
            hash_alg: None,
        };
        assert!(matches!(
            handler.check_server_key(&ask(listed)).await,
            Ok(true)
        ));
        assert!(matches!(
            handler.check_server_key(&ask(other)).await,
            Ok(false)
        ));
    }

    /// A server that accepts one client key and answers `git-upload-pack` like Git does: the
    /// advertisement, then a wait for the client's wants.
    mod server {
        use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
        use russh::{Channel, ChannelId};

        use super::*;

        pub(super) const COMMAND: &str = "git-upload-pack '/acme/api.git'";

        pub(super) struct Git {
            pub(super) client: PublicKey,
            pub(super) advertisement: Vec<u8>,
        }

        impl server::Handler for Git {
            type Error = russh::Error;

            fn auth_publickey(
                &mut self,
                _user: &str,
                key: VerifiedPublicKey<'_>,
            ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
                let auth = if key.public_key.key_data() == self.client.key_data() {
                    Auth::Accept
                } else {
                    Auth::reject()
                };
                std::future::ready(Ok(auth))
            }

            async fn channel_open_session(
                &mut self,
                _channel: Channel<Msg>,
                reply: ChannelOpenHandle,
                _session: &mut Session,
            ) -> Result<(), Self::Error> {
                reply.accept().await;
                Ok(())
            }

            fn exec_request(
                &mut self,
                channel: ChannelId,
                data: &[u8],
                session: &mut Session,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                let result = if data == COMMAND.as_bytes() {
                    session
                        .channel_success(channel)
                        .and_then(|()| session.data(channel, self.advertisement.clone()))
                } else {
                    session.channel_failure(channel)
                };
                std::future::ready(result)
            }
        }
    }

    fn key(seed: u8) -> russh::keys::PrivateKey {
        russh::keys::PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
            &[seed; 32],
        ))
    }

    fn openssh(key: &russh::keys::PrivateKey) -> String {
        key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .map_or_else(|_| String::new(), |k| k.to_string())
    }

    /// Serves one connection on loopback; the `ssh://` URL to reach it.
    async fn serve(host_key: u8, client_key: u8, advertisement: Vec<u8>) -> String {
        let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
            panic!("no loopback port");
        };
        let port = listener.local_addr().map_or(0, |a| a.port());
        let config = Arc::new(russh::server::Config {
            keys: vec![key(host_key)],
            auth_rejection_time: Duration::ZERO,
            ..russh::server::Config::default()
        });
        let handler = server::Git {
            client: key(client_key).public_key().clone(),
            advertisement,
        };
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await
                && let Ok(session) = russh::server::run_stream(config, stream, handler).await
            {
                drop(session.await);
            }
        });
        format!("ssh://git@127.0.0.1:{port}/acme/api.git")
    }

    fn known_hosts(url: &str, host_key: u8) -> String {
        let port = url
            .split(':')
            .nth(2)
            .and_then(|p| p.split('/').next())
            .unwrap_or_default();
        let public = key(host_key).public_key().to_openssh().unwrap_or_default();
        format!("[127.0.0.1]:{port} {public}\n")
    }

    const SHA: &str = "3e1a9c2d4f0b5a6c7d8e9f0a1b2c3d4e5f6a7b8c";

    fn refs() -> Vec<u8> {
        let mut out = Vec::new();
        for line in [
            format!("{SHA} HEAD\0multi_ack symref=HEAD:refs/heads/main\n"),
            format!("{SHA} refs/heads/main\n"),
        ] {
            out.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
        }
        out.extend_from_slice(b"0000");
        out
    }

    async fn ask(url: &str, client_key: u8, known_hosts: &str) -> Result<Vec<u8>, ResolveError> {
        let remote = SshRemote::parse(url).unwrap_or_else(|| remote(""));
        advertisement(&remote, &openssh(&key(client_key)), known_hosts).await
    }

    #[tokio::test]
    async fn reads_the_advertisement() {
        let url = serve(1, 2, refs()).await;
        let answer = ask(&url, 2, &known_hosts(&url, 1)).await;
        assert_eq!(answer.ok(), Some(refs()));
    }

    #[tokio::test]
    async fn refuses_another_host_key() {
        let url = serve(1, 2, refs()).await;
        let answer = ask(&url, 2, &known_hosts(&url, 3)).await;
        assert!(
            matches!(answer, Err(ResolveError::HostKey(_))),
            "{answer:?}"
        );
    }

    #[tokio::test]
    async fn reports_a_refused_client_key() {
        let url = serve(1, 2, refs()).await;
        let answer = ask(&url, 4, &known_hosts(&url, 1)).await;
        assert!(matches!(answer, Err(ResolveError::Denied(_))), "{answer:?}");
    }
}
