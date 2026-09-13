use std::collections::BTreeMap;

const REQUIRED_COLUMNS: [&str; 15] = [
    "frame",
    "time",
    "urb_type",
    "transfer_type",
    "bus",
    "address",
    "endpoint",
    "bmRequestType",
    "bRequest",
    "wValue",
    "wIndex",
    "wLength",
    "data_length",
    "request_data",
    "response_data",
];

#[derive(Debug, Default, Eq, PartialEq)]
pub struct EndpointSummary {
    pub records: u64,
    pub payload_records: u64,
    pub bytes: u64,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct TraceSummary {
    pub rows: u64,
    pub class_interface_records: u64,
    pub classifications: BTreeMap<String, u64>,
    pub endpoints: BTreeMap<u8, EndpointSummary>,
}

impl TraceSummary {
    pub fn render(&self) -> String {
        let mut output = format!(
            "trace rows: {}\nclass-interface records: {}\nclassifications:\n",
            self.rows, self.class_interface_records
        );
        if self.classifications.is_empty() {
            output.push_str("  none\n");
        } else {
            for (classification, count) in &self.classifications {
                output.push_str(&format!("  {classification}: {count}\n"));
            }
        }

        output.push_str("endpoints:\n");
        if self.endpoints.is_empty() {
            output.push_str("  none\n");
        } else {
            for (endpoint, summary) in &self.endpoints {
                let name = match endpoint {
                    0x81 => "interrupt",
                    0x83 => "stream",
                    _ => "other",
                };
                output.push_str(&format!(
                    "  0x{endpoint:02x} {name}: records={} payload_records={} bytes={}\n",
                    summary.records, summary.payload_records, summary.bytes
                ));
            }
        }
        output
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct TraceAnalysis {
    pub sanitized_tsv: String,
    pub summary: TraceSummary,
}

pub fn analyze_tsv(input: &str) -> Result<TraceAnalysis, String> {
    let mut lines = input.lines();
    let header = lines
        .next()
        .ok_or_else(|| "trace is empty; expected a TSV header".to_owned())?
        .trim_end_matches('\r');
    let columns = header.split('\t').collect::<Vec<_>>();
    if columns.contains(&"classification") {
        return Err("input already contains a classification column".to_owned());
    }

    let mut indices = BTreeMap::new();
    for required in REQUIRED_COLUMNS {
        let matches = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| **column == required)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [index] => {
                indices.insert(required, *index);
            }
            [] => return Err(format!("missing required TSV column {required:?}")),
            _ => return Err(format!("duplicate TSV column {required:?}")),
        }
    }

    let mut sanitized = format!("{header}\tclassification\n");
    let mut summary = TraceSummary::default();
    for (offset, raw_line) in lines.enumerate() {
        let line_number = offset + 2;
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            return Err(format!("line {line_number} is empty"));
        }
        let mut fields = line.split('\t').map(str::to_owned).collect::<Vec<_>>();
        if fields.len() != columns.len() {
            return Err(format!(
                "line {line_number} has {} fields; expected {}",
                fields.len(),
                columns.len()
            ));
        }

        let endpoint = optional_number(field(&fields, &indices, "endpoint"), line_number)?;
        let request = optional_number(field(&fields, &indices, "bRequest"), line_number)?;
        let value = optional_number(field(&fields, &indices, "wValue"), line_number)?;
        let bm_request_type =
            optional_number(field(&fields, &indices, "bmRequestType"), line_number)?;
        let data_length =
            optional_number(field(&fields, &indices, "data_length"), line_number)?.unwrap_or(0);

        let classification = classify(
            endpoint,
            bm_request_type,
            request,
            value,
            field(&fields, &indices, "request_data"),
        );
        summary.rows += 1;
        *summary
            .classifications
            .entry(classification.to_owned())
            .or_default() += 1;

        // The driver's register transport shows up on the wire as vendor/device
        // (0x40/0xc0) rather than class/interface (0x21/0xa1); count both.
        if bm_request_type
            .map(|request_type| matches!(request_type & 0x7f, 0x21 | 0x40))
            .unwrap_or(false)
        {
            summary.class_interface_records += 1;
        }

        if matches!(endpoint, Some(0x81 | 0x83)) {
            let endpoint = endpoint.unwrap() as u8;
            let endpoint_summary = summary.endpoints.entry(endpoint).or_default();
            endpoint_summary.records += 1;
            endpoint_summary.bytes += data_length;
            if data_length > 0
                || !field(&fields, &indices, "request_data").is_empty()
                || !field(&fields, &indices, "response_data").is_empty()
            {
                endpoint_summary.payload_records += 1;
            }
        }

        if !classification.starts_with("volatile-register-") {
            let marker = format!("<redacted:{classification}>");
            redact_field(&mut fields, &indices, "request_data", &marker);
            redact_field(&mut fields, &indices, "response_data", &marker);
        }

        sanitized.push_str(&fields.join("\t"));
        sanitized.push('\t');
        sanitized.push_str(classification);
        sanitized.push('\n');
    }

    Ok(TraceAnalysis {
        sanitized_tsv: sanitized,
        summary,
    })
}

fn field<'a>(fields: &'a [String], indices: &BTreeMap<&str, usize>, name: &str) -> &'a str {
    &fields[indices[name]]
}

fn optional_number(value: &str, line_number: usize) -> Result<Option<u64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let parsed = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse::<u64>()
    };
    parsed
        .map(Some)
        .map_err(|_| format!("line {line_number} contains invalid number {value:?}"))
}

fn classify(
    endpoint: Option<u64>,
    bm_request_type: Option<u64>,
    request: Option<u64>,
    value: Option<u64>,
    request_data: &str,
) -> &'static str {
    match endpoint {
        Some(0x81) => return "endpoint-interrupt-0x81",
        Some(0x83) => return "endpoint-stream-0x83",
        _ => {}
    }

    let direction_in = bm_request_type
        .map(|request_type| request_type & 0x80 != 0)
        .unwrap_or(false);
    match (request, value, direction_in) {
        (Some(0xc0), Some(0x0098), _) => "volatile-register-direct-bank-0x0098",
        (Some(0xc0), Some(0x009c), _) => "volatile-register-direct-bank-0x009c",
        // Rev. 4 (0fd9:0076) uses bank 0x64 where Rev. 2 uses 0x98; seen live
        // as vendor/device requests 0x40/0xc0 in a usbmon trace.
        (Some(0xc0), Some(0x0064), _) => "volatile-register-direct-bank-0x0064",
        (Some(0xc0), Some(0x5064), false) => "volatile-register-proxy-write-bank-0x0064",
        (Some(0xc0), Some(0x5066), false) if first_payload_byte(request_data) == Some(0x65) => {
            "volatile-register-proxy-read-bank-0x0064"
        }
        (Some(0xc0), Some(0x5098), false) => "volatile-register-proxy-write-bank-0x0098",
        (Some(0xc0), Some(0x509c), false) => "volatile-register-proxy-write-bank-0x009c",
        (Some(0xc0), Some(0x5066), false) if first_payload_byte(request_data) == Some(0x99) => {
            "volatile-register-proxy-read-bank-0x0098"
        }
        (Some(0xc0), Some(0x5066), false) if first_payload_byte(request_data) == Some(0x9d) => {
            "volatile-register-proxy-read-bank-0x009c"
        }
        (Some(0xa0), _, _) => "sensitive-eeprom",
        (Some(0xa6), _, _) => "sensitive-board-memory",
        (Some(0xc1), Some(0x0039), _) => "sensitive-internal-register",
        (Some(_), _, _) => "unknown-class-request",
        (None, _, _) => "unknown-record",
    }
}

fn first_payload_byte(value: &str) -> Option<u8> {
    let hex = value
        .chars()
        .filter(|character| !matches!(character, ':' | '-' | ' '))
        .collect::<String>();
    if hex.len() < 2 {
        return None;
    }
    u8::from_str_radix(&hex[..2], 16).ok()
}

fn redact_field(fields: &mut [String], indices: &BTreeMap<&str, usize>, name: &str, marker: &str) {
    let field = &mut fields[indices[name]];
    if !field.is_empty() {
        *field = marker.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_numeric_fields() {
        let input = "frame\ttime\turb_type\ttransfer_type\tbus\taddress\tendpoint\tbmRequestType\tbRequest\twValue\twIndex\twLength\tdata_length\trequest_data\tresponse_data\n1\t0\tS\t2\t1\t2\tbogus\t\t\t\t\t\t\t\t\n";
        assert_eq!(
            analyze_tsv(input).unwrap_err(),
            "line 2 contains invalid number \"bogus\""
        );
    }

    #[test]
    fn parses_common_tshark_payload_formats() {
        assert_eq!(first_payload_byte("99:01:3b"), Some(0x99));
        assert_eq!(first_payload_byte("9d 01 20"), Some(0x9d));
        assert_eq!(first_payload_byte(""), None);
    }
}
