use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SignatureInfo {
    pub index: usize,
    pub signer_name: Option<String>,
    pub distinguished_name: Option<String>,
    pub signing_time: Option<String>,
    pub hash_algorithm: Option<String>,
    pub signature_type: Option<String>,
    pub signature_validation: Option<String>,
    pub certificate_validation: Option<String>,
    pub valid: Option<bool>,
    pub raw_lines: Vec<String>,
}

impl SignatureInfo {
    fn new(index: usize) -> Self {
        Self {
            index,
            signer_name: None,
            distinguished_name: None,
            signing_time: None,
            hash_algorithm: None,
            signature_type: None,
            signature_validation: None,
            certificate_validation: None,
            valid: None,
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
                signatures.push(signature);
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
            continue;
        };

        let value = value.trim().to_owned();
        match name.trim() {
            "Signer Certificate Common Name" => signature.signer_name = Some(value),
            "Signer full Distinguished Name" => signature.distinguished_name = Some(value),
            "Signing Time" => signature.signing_time = Some(value),
            "Signing Hash Algorithm" => signature.hash_algorithm = Some(value),
            "Signature Type" => signature.signature_type = Some(value),
            "Signature Validation" => {
                signature.valid = signature_validity(&value);
                signature.signature_validation = Some(value);
            }
            "Certificate Validation" => signature.certificate_validation = Some(value),
            _ => {}
        }
    }

    if let Some(signature) = current.take() {
        signatures.push(signature);
    }

    signatures
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

pub fn signatures_note(signatures: &[SignatureInfo]) -> String {
    let mut note = String::from("Podpis cyfrowy wykryty przez pdf-sign-check-rs.\n");

    for signature in signatures {
        note.push('\n');
        note.push_str(&format!("Podpis #{}\n", signature.index));
        note.push_str(&format!(
            "Kto: {}\n",
            signature
                .signer_name
                .as_deref()
                .or(signature.distinguished_name.as_deref())
                .unwrap_or("brak danych")
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
    }

    note
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
        assert_eq!(signatures[0].signer_name.as_deref(), Some("Jan Kowalski"));
        assert_eq!(signatures[0].valid, Some(true));
    }

    #[test]
    fn does_not_parse_no_signature_message_as_signature() {
        let signatures = parse_pdfsig_output("File 'x.pdf' does not contain any signatures\n");
        assert!(signatures.is_empty());
    }
}
