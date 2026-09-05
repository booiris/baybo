const GATEWAY_KEY_PREFIX: &str = "gateway-";

pub(crate) fn active_key() -> Result<Option<String>, String> {
    let leg = match crate::binding::active_leg() {
        Ok(leg) => leg,
        Err(error) if error == crate::binding::NOT_BOUND_MSG => return Ok(None),
        Err(error) => return Err(error),
    };
    let key = match leg {
        crate::binding::ActiveLeg::Relay => crate::relay::pairing::load_paired_record()?
            .map(|record| gateway_key(&record.gateway_static_pubkey)),
        crate::binding::ActiveLeg::Direct => crate::direct::credentials()?
            .map(|credentials| direct_key(&credentials))
            .transpose()?,
    };
    Ok(key)
}

pub(crate) fn gateway_key(public_key: &[u8; 32]) -> String {
    format!("{GATEWAY_KEY_PREFIX}{}", hex::encode(public_key))
}

pub(crate) fn gateway_key_from_hex(public_key: &str) -> Option<String> {
    let bytes = hex::decode(public_key).ok()?;
    let public_key: [u8; 32] = bytes.try_into().ok()?;
    Some(gateway_key(&public_key))
}

pub(crate) fn direct_key(credentials: &crate::direct::DirectCredentials) -> Result<String, String> {
    gateway_key_from_hex(&credentials.server_key)
        .ok_or_else(|| "stored direct credentials have no valid server key; sign in again".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_binding_modes_use_the_gateway_public_identity() {
        let public_key = [7u8; 32];
        let direct = crate::direct::DirectCredentials {
            base_url: "https://gw.example".to_string(),
            token: "token".to_string(),
            server_key: hex::encode(public_key),
        };

        assert_eq!(direct_key(&direct), Ok(gateway_key(&public_key)));
    }
}
