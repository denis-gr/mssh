use std::io::{Read, Write};

use sequoia_openpgp::serialize::stream::Armorer;
use sequoia_openpgp::serialize::stream::Signer;
use sequoia_openpgp::{self as openpgp};

use log;
use openpgp::crypto::{Password, SessionKey};
use openpgp::parse::Parse;
use openpgp::parse::stream::{
    DecryptionHelper, DecryptorBuilder, MessageLayer, MessageStructure, VerificationHelper,
};
use openpgp::policy::StandardPolicy;
use openpgp::serialize::stream::{Encryptor, LiteralWriter, Message};
use openpgp::types::SymmetricAlgorithm;
use openpgp::{Cert, KeyHandle};

use std::fs;
use std::iter::once;
use std::str::FromStr;

use anyhow::Error;
use anyhow::anyhow;
use std::collections::HashMap;

fn email(cert: &Cert) -> Option<&str> {
    cert.userids()
        .next()
        .and_then(|u| Some(u.userid().email().ok()??))
}

#[derive(Clone)]
pub struct Helper {
    server_cert: Cert,
    client_certs: HashMap<String, Cert>,
    password: Option<Password>,
}

impl Helper {
    pub fn load_dir(path: &str, pgp_password: Option<String>) -> Result<Self, Error> {
        let dir = fs::read_dir(path)?;
        let certs = dir
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let content = fs::read_to_string(path).ok()?;
                Cert::from_str(&content).ok()
            })
            .collect::<Vec<Cert>>();
        let mut server_certs = certs.iter().filter(|c| c.is_tsk());
        let server_cert = server_certs
            .next()
            .ok_or(anyhow::anyhow!("No server cert"))?
            .clone();
        let client_certs = certs
            .into_iter()
            .filter(|c| c != &server_cert)
            .collect::<Vec<Cert>>();
        once(server_cert.clone())
            .chain(client_certs.iter().cloned())
            .for_each(|c| {
                log::info!("Cert: {} {}", email(&c).unwrap_or("..."), c.fingerprint());
            });
        Ok(Self {
            server_cert,
            client_certs: client_certs
                .into_iter()
                .filter_map(|c| {
                    let addr = email(&c)?.to_string();
                    Some((addr, c))
                })
                .collect(),
            password: pgp_password.map(Password::from),
        })
    }

    /*pub fn has_client_cert(&self, email: &str) -> bool {
        self.client_certs.contains_key(email)
    }*/

    pub fn decrypt_message(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let policy = StandardPolicy::new();
        let helper: Helper = self.clone();

        let mut decryptor =
            DecryptorBuilder::from_bytes(message)?.with_policy(&policy, None, helper)?;

        let mut decrypted_message = Vec::new();
        decryptor.read_to_end(&mut decrypted_message)?;

        Ok(decrypted_message)
    }

    pub fn encrypt_message(&self, message: &[u8], email: String) -> Result<Vec<u8>, Error> {
        let recipient_cert = self
            .client_certs
            .get(&email)
            .ok_or_else(|| Error::msg(format!("No client cert found for {email}")))?;

        let policy = StandardPolicy::new();
        let recipients = recipient_cert
            .keys()
            .with_policy(&policy, None)
            .supported()
            .alive()
            .revoked(false)
            .for_transport_encryption();

        let signing_key = self
            .server_cert
            .keys()
            .secret()
            .with_policy(&policy, None)
            .supported()
            .alive()
            .revoked(false)
            .for_signing()
            .next()
            .ok_or_else(|| Error::msg("No suitable server signing key found"))?
            .key()
            .clone();

        let mut signing_key = signing_key;
        if !signing_key.has_unencrypted_secret() {
            let p = self
                .password
                .as_ref()
                .ok_or_else(|| Error::msg("Signing key is encrypted; set password"))?;
            signing_key = signing_key.decrypt_secret(p)?;
        }

        let signing_key = signing_key.into_keypair()?;

        let mut sink = Vec::new();
        let message_stream = Message::new(&mut sink);
        let message_stream = Armorer::new(message_stream).build()?;
        let message_stream = Encryptor::for_recipients(message_stream, recipients).build()?;
        let message_stream = Signer::new(message_stream, signing_key)?.build()?;
        let mut writer = LiteralWriter::new(message_stream).build()?;

        writer.write_all(message)?;
        writer.finalize()?;

        Ok(sink)
    }
}

impl DecryptionHelper for Helper {
    fn decrypt(
        &mut self,
        pkesks: &[openpgp::packet::PKESK],
        _: &[openpgp::packet::SKESK],
        sym_algo: Option<SymmetricAlgorithm>,
        decrypt: &mut dyn FnMut(Option<SymmetricAlgorithm>, &SessionKey) -> bool,
    ) -> openpgp::Result<Option<Cert>> {
        let policy = StandardPolicy::new();

        for key in self
            .server_cert
            .keys()
            .secret()
            .with_policy(&policy, None)
            .supported()
            .alive()
            .revoked(false)
            .for_transport_encryption()
        {
            let mut secret_key = key.key().clone();
            if !secret_key.has_unencrypted_secret() {
                let Some(password) = self.password.as_ref() else {
                    continue;
                };
                secret_key = secret_key.decrypt_secret(password)?;
            }

            let mut keypair = match secret_key.into_keypair() {
                Ok(kp) => kp,
                Err(_) => continue,
            };

            for pkesk in pkesks {
                let ok = pkesk
                    .decrypt(&mut keypair, sym_algo)
                    .map(|(algo, session_key)| decrypt(algo, &session_key))
                    .unwrap_or(false);

                if ok {
                    return Ok(Some(self.server_cert.clone()));
                }
            }
        }

        Ok(None)
    }
}

impl VerificationHelper for Helper {
    fn get_certs(&mut self, ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        let all_certs: Vec<Cert> = once(self.server_cert.clone())
            .chain(self.client_certs.values().cloned())
            .collect();

        if ids.is_empty() {
            return Ok(all_certs);
        }

        let matches_id = |cert: &Cert, id: &KeyHandle| {
            let cert_fpr: KeyHandle = cert.fingerprint().into();
            let cert_keyid: KeyHandle = cert.keyid().into();
            id.aliases(&cert_fpr)
                || id.aliases(&cert_keyid)
                || cert.keys().any(|ka| {
                    let key_fpr: KeyHandle = ka.key().fingerprint().into();
                    let key_keyid: KeyHandle = ka.key().keyid().into();
                    id.aliases(&key_fpr) || id.aliases(&key_keyid)
                })
        };

        Ok(all_certs
            .into_iter()
            .filter(|cert| ids.iter().any(|id| matches_id(cert, id)))
            .collect())
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        let mut saw_signature_layer = false;

        for layer in structure.into_iter() {
            if let MessageLayer::SignatureGroup { results } = layer {
                saw_signature_layer = true;
                if !results.iter().any(|r| r.is_ok()) {
                    return Err(anyhow!("No valid signature"));
                }
            }
        }

        if !saw_signature_layer {
            return Err(anyhow!("Message is not signed"));
        }

        Ok(())
    }
}
