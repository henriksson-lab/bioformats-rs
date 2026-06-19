use quick_xml::encoding::Decoder;
use quick_xml::events::{attributes::Attribute, BytesRef, BytesText};

pub fn decode_xml_text(text: &BytesText<'_>) -> Option<String> {
    let decoded = text.xml_content(quick_xml::XmlVersion::Implicit1_0).ok()?;
    Some(
        quick_xml::escape::unescape(decoded.as_ref())
            .map(|value| value.into_owned())
            .unwrap_or_else(|_| decoded.into_owned()),
    )
}

pub fn decode_xml_ref(reference: &BytesRef<'_>) -> Option<String> {
    let name = reference
        .xml_content(quick_xml::XmlVersion::Implicit1_0)
        .ok()?;
    let decoded = match name.as_ref() {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "apos" => "'".into(),
        "quot" => "\"".into(),
        value if value.starts_with("#x") => u32::from_str_radix(&value[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())?,
        value if value.starts_with('#') => value[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())?,
        value => format!("&{value};"),
    };
    Some(decoded)
}

pub fn decode_xml_attr(attr: Attribute<'_>, decoder: Decoder) -> Option<String> {
    attr.decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
        .ok()
        .map(|value| value.into_owned())
}

pub fn decode_xml_escaped_str(value: &str) -> String {
    quick_xml::escape::unescape(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use quick_xml::events::Event;

    #[test]
    fn text_and_general_refs_reconstruct_java_text_content() {
        let mut reader = quick_xml::Reader::from_str("<x>Ada &amp; Co &#181;</x>");
        reader.config_mut().trim_text(false);
        let mut text = String::new();

        loop {
            match reader.read_event() {
                Ok(Event::Text(t)) => {
                    text.push_str(&super::decode_xml_text(&t).unwrap());
                }
                Ok(Event::GeneralRef(r)) => {
                    text.push_str(&super::decode_xml_ref(&r).unwrap());
                }
                Ok(Event::End(_)) | Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("XML parse failed: {e}"),
            }
        }

        assert_eq!(text, "Ada & Co \u{b5}");
    }

    #[test]
    fn attributes_are_decoded_and_normalized() {
        let mut reader =
            quick_xml::Reader::from_str(r#"<x filename="Ada &amp; Co.tif" unit="&#181;m"/>"#);
        let decoder = reader.decoder();
        match reader.read_event() {
            Ok(Event::Empty(e)) => {
                let attrs: Vec<_> = e
                    .attributes()
                    .flatten()
                    .filter_map(|attr| super::decode_xml_attr(attr, decoder))
                    .collect();
                assert_eq!(attrs, ["Ada & Co.tif", "\u{b5}m"]);
            }
            event => panic!("unexpected event: {event:?}"),
        }
    }

    #[test]
    fn escaped_strings_decode_like_sax_values() {
        assert_eq!(
            super::decode_xml_escaped_str("DAPI &amp; FITC &#181;m"),
            "DAPI & FITC \u{b5}m"
        );
    }
}
