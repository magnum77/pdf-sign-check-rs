use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SignatureInfo {
    pub index: usize,
    pub field_name: Option<String>,
    pub signer_name: Option<String>,
    pub distinguished_name: Option<String>,
    pub signing_time: Option<String>,
    pub hash_algorithm: Option<String>,
    pub signature_type: Option<String>,
    pub signed_ranges: Option<String>,
    pub total_document_signed: Option<bool>,
    pub signature_validation: Option<String>,
    pub certificate_validation: Option<String>,
    pub valid: Option<bool>,
    pub ignored: bool,
    pub ignored_reason: Option<String>,
    pub raw_lines: Vec<String>,
}

impl SignatureInfo {
    fn new(index: usize) -> Self {
        Self {
            index,
            field_name: None,
            signer_name: None,
            distinguished_name: None,
            signing_time: None,
            hash_algorithm: None,
            signature_type: None,
            signed_ranges: None,
            total_document_signed: None,
            signature_validation: None,
            certificate_validation: None,
            valid: None,
            ignored: false,
            ignored_reason: None,
            raw_lines: Vec::new(),
        }
    }
}

pub fn parse_pdfsig_output(stdout: &str) -> Vec<SignatureInfo> {
    let mut signatures = Vec::new();
    let mut current: Option<SignatureInfo> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(index) = signature_index(trimmed) {
            if let Some(signature) = current.take() {
                signatures.push(finalize_signature(signature));
            }
            current = Some(SignatureInfo::new(index));
            continue;
        }

        let Some(signature) = current.as_mut() else {
            continue;
        };

        signature.raw_lines.push(trimmed.to_owned());
        let field = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let Some((name, value)) = field.split_once(':') else {
            match field.trim() {
                "Total document signed" => signature.total_document_signed = Some(true),
                "Not total document signed" => signature.total_document_signed = Some(false),
                _ => {}
            }
            continue;
        };

        let value = clean_value(value);
        match name.trim() {
            "Signature Field Name" => signature.field_name = value,
            "Signer Certificate Common Name" => signature.signer_name = value,
            "Signer full Distinguished Name" => signature.distinguished_name = value,
            "Signing Time" => signature.signing_time = clean_signing_time(value),
            "Signing Hash Algorithm" => signature.hash_algorithm = value,
            "Signature Type" => signature.signature_type = value,
            "Signed Ranges" => signature.signed_ranges = value,
            "Signature Validation" => {
                if let Some(value) = value {
                    signature.valid = signature_validity(&value);
                    signature.signature_validation = Some(value);
                }
            }
            "Certificate Validation" => signature.certificate_validation = value,
            _ => {}
        }
    }

    if let Some(signature) = current.take() {
        signatures.push(finalize_signature(signature));
    }

    signatures
}

pub fn actionable_signatures(signatures: &[SignatureInfo]) -> Vec<SignatureInfo> {
    signatures
        .iter()
        .filter(|signature| !signature.ignored)
        .cloned()
        .collect()
}

fn signature_index(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Signature #")?;
    let number = rest.trim_end_matches(':').trim();
    number.parse().ok()
}

fn signature_validity(value: &str) -> Option<bool> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("not valid") || lower.contains("invalid") {
        Some(false)
    } else if lower.contains("valid") {
        Some(true)
    } else {
        None
    }
}

fn clean_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn clean_signing_time(value: Option<String>) -> Option<String> {
    let value = value?;
    if value == "Jan 01 1970 00:00:00" {
        None
    } else {
        Some(value)
    }
}

fn finalize_signature(mut signature: SignatureInfo) -> SignatureInfo {
    if is_placeholder_signature(&signature) {
        signature.ignored = true;
        signature.ignored_reason = Some(
            "empty signer, placeholder time, unknown algorithm/type and not verified".to_owned(),
        );
    }
    signature
}

fn is_placeholder_signature(signature: &SignatureInfo) -> bool {
    signature.signer_name.is_none()
        && signature.distinguished_name.is_none()
        && signature.signing_time.is_none()
        && signature.hash_algorithm.is_none()
        && signature.signature_type.is_none()
        && signature
            .signature_validation
            .as_deref()
            .map(|value| {
                value
                    .to_ascii_lowercase()
                    .contains("has not yet been verified")
            })
            .unwrap_or(false)
}

pub fn signatures_note(signatures: &[SignatureInfo]) -> String {
    let mut note = String::from("Podpis cyfrowy wykryty przez pdf-sign-check-rs.\n");

    for signature in signatures.iter().filter(|signature| !signature.ignored) {
        note.push('\n');
        match &signature.field_name {
            Some(field_name) => {
                note.push_str(&format!("Podpis #{} ({field_name})\n", signature.index))
            }
            None => note.push_str(&format!("Podpis #{}\n", signature.index)),
        }
        note.push_str(&format!(
            "Kto: {}\n",
            signature_signer(signature).unwrap_or("brak danych")
        ));
        note.push_str(&format!(
            "Czas podpisania: {}\n",
            signature.signing_time.as_deref().unwrap_or("brak danych")
        ));
        note.push_str(&format!(
            "Poprawność podpisu: {}\n",
            signature
                .signature_validation
                .as_deref()
                .unwrap_or("brak danych")
        ));
        note.push_str(&format!(
            "Poprawność certyfikatu: {}\n",
            signature
                .certificate_validation
                .as_deref()
                .unwrap_or("brak danych")
        ));
        if let Some(hash_algorithm) = &signature.hash_algorithm {
            note.push_str(&format!("Algorytm skrótu: {hash_algorithm}\n"));
        }
        if let Some(signature_type) = &signature.signature_type {
            note.push_str(&format!("Typ podpisu: {signature_type}\n"));
        }
        if let Some(total_document_signed) = signature.total_document_signed {
            let value = if total_document_signed { "tak" } else { "nie" };
            note.push_str(&format!("Podpis obejmuje cały dokument: {value}\n"));
        }
        if let Some(signed_ranges) = &signature.signed_ranges {
            note.push_str(&format!("Zakresy podpisu: {signed_ranges}\n"));
        }
    }

    note
}

fn signature_signer(signature: &SignatureInfo) -> Option<&str> {
    signature
        .signer_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            signature
                .distinguished_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pdfsig_signature_sections() {
        let stdout = r#"Digital Signature Info of: signed.pdf
Signature #1:
  - Signer Certificate Common Name: Jan Kowalski
  - Signer full Distinguished Name: CN=Jan Kowalski,O=Example
  - Signing Time: May 13 2026 12:00:00
  - Signing Hash Algorithm: SHA-256
  - Signature Type: adbe.pkcs7.detached
  - Signature Validation: Signature is Valid.
  - Certificate Validation: Certificate is Trusted.
"#;

        let signatures = parse_pdfsig_output(stdout);
        assert_eq!(signatures.len(), 1);
        assert_eq!(signatures[0].index, 1);
        assert_eq!(signatures[0].field_name.as_deref(), None);
        assert_eq!(signatures[0].signer_name.as_deref(), Some("Jan Kowalski"));
        assert_eq!(signatures[0].valid, Some(true));
    }

    #[test]
    fn does_not_parse_no_signature_message_as_signature() {
        let signatures = parse_pdfsig_output("File 'x.pdf' does not contain any signatures\n");
        assert!(signatures.is_empty());
    }

    #[test]
    fn parses_empty_and_unknown_signature_fields_as_missing_data() {
        let stdout = r#"Signature #2:
  - Signature Field Name: Signature1
  - Signer Certificate Common Name: Przemysław Narloch
  - Signer full Distinguished Name: C=PL,serialNumber=PNOPL-79070719394,SN=Narloch,givenName=Przemysław,CN=Przemysław Narloch
  - Signing Time: May 05 2026 12:21:12
  - Signing Hash Algorithm: SHA-256
  - Signature Type: ETSI.CAdES.detached
  - Signed Ranges: [0 - 977403], [1002173 - 1033657]
  - Not total document signed
  - Signature Validation: Signature is Valid.
  - Certificate Validation: Certificate issuer is unknown.
Signature #3:
  - Signature Field Name: Signature3
  - Signer Certificate Common Name:
  - Signer full Distinguished Name:
  - Signing Time: Jan 01 1970 00:00:00
  - Signing Hash Algorithm: unknown
  - Signature Type: unknown
  - Signed Ranges: [0 - 1252574], [1268960 - 1273872]
  - Total document signed
  - Signature Validation: Signature has not yet been verified.
"#;

        let signatures = parse_pdfsig_output(stdout);
        assert_eq!(signatures.len(), 2);
        assert_eq!(signatures[0].field_name.as_deref(), Some("Signature1"));
        assert_eq!(signatures[0].total_document_signed, Some(false));
        assert_eq!(signatures[0].valid, Some(true));
        assert_eq!(signatures[1].field_name.as_deref(), Some("Signature3"));
        assert_eq!(signatures[1].signer_name, None);
        assert_eq!(signatures[1].distinguished_name, None);
        assert_eq!(signatures[1].signing_time, None);
        assert_eq!(signatures[1].hash_algorithm, None);
        assert_eq!(signatures[1].signature_type, None);
        assert_eq!(signatures[1].total_document_signed, Some(true));
        assert_eq!(signatures[1].valid, None);
        assert!(signatures[1].ignored);

        let note = signatures_note(&signatures);
        assert!(!note.contains("Podpis #3 (Signature3)"));
        assert!(!note.contains("Kto: brak danych"));
        assert!(note.contains("Podpis obejmuje cały dokument: nie"));
    }

    #[test]
    fn actionable_signatures_skip_placeholder_entries() {
        let stdout = r#"Signature #3:
  - Signature Field Name: Signature3
  - Signer Certificate Common Name:
  - Signer full Distinguished Name:
  - Signing Time: Jan 01 1970 00:00:00
  - Signing Hash Algorithm: unknown
  - Signature Type: unknown
  - Signed Ranges: [0 - 1252574], [1268960 - 1273872]
  - Total document signed
  - Signature Validation: Signature has not yet been verified.
"#;

        let signatures = parse_pdfsig_output(stdout);
        assert_eq!(signatures.len(), 1);
        assert!(actionable_signatures(&signatures).is_empty());
        assert!(!signatures_note(&signatures).contains("Podpis #3"));
    }
}
