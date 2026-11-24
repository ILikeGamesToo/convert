// SPDX-License-Identifier: GPL-2.0-or-later

//! CSV, JSON, YAML and TOML conversions.

use crate::error::ConvertError;
use std::collections::{BTreeSet, HashSet};

pub(crate) fn csv_to_json(text: &str) -> Result<serde_json::Value, ConvertError> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers()?.clone();
    let mut seen_headers = HashSet::new();
    for header in &headers {
        if !seen_headers.insert(header) {
            return Err(ConvertError::DuplicateCsvHeader);
        }
    }

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        let mut object = serde_json::Map::new();
        for (header, value) in headers.iter().zip(record.iter()) {
            object.insert(
                header.to_owned(),
                serde_json::Value::String(value.to_owned()),
            );
        }
        rows.push(serde_json::Value::Object(object));
    }
    Ok(serde_json::Value::Array(rows))
}

pub(crate) fn json_to_csv(value: &serde_json::Value) -> Result<Vec<u8>, ConvertError> {
    let rows = value.as_array().ok_or(ConvertError::JsonNotTabular)?;
    if rows.is_empty() {
        return Err(ConvertError::JsonNotTabular);
    }

    let mut headers = BTreeSet::new();
    for row in rows {
        let object = row.as_object().ok_or(ConvertError::JsonNotTabular)?;
        headers.extend(object.keys().cloned());
    }
    if headers.is_empty() {
        return Err(ConvertError::JsonNotTabular);
    }
    let headers: Vec<_> = headers.into_iter().collect();

    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&headers)?;
    for row in rows {
        let object = row.as_object().ok_or(ConvertError::JsonNotTabular)?;
        let record: Vec<String> = headers
            .iter()
            .map(|header| match object.get(header) {
                None | Some(serde_json::Value::Null) => String::new(),
                Some(serde_json::Value::String(value)) => value.clone(),
                Some(value) => value.to_string(),
            })
            .collect();
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(writer.into_inner().map_err(|error| error.into_error())?)
}

pub(crate) fn text_to_csv(text: &str) -> Result<Vec<u8>, ConvertError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["text"])?;
    for line in text.lines() {
        writer.write_record([line])?;
    }
    writer.flush()?;
    Ok(writer.into_inner().map_err(|error| error.into_error())?)
}

pub(crate) fn parse_data_to_json(
    extension: &str,
    text: &str,
) -> Result<serde_json::Value, ConvertError> {
    match extension {
        "json" => Ok(serde_json::from_str(text)?),
        "yaml" | "yml" => Ok(serde_yaml_ng::from_str(text)?),
        "toml" => {
            let value: toml::Value = toml::from_str(text)?;
            Ok(toml_to_json(value))
        }
        "csv" => csv_to_json(text),
        _ => Ok(serde_json::Value::String(text.to_owned())),
    }
}

pub(crate) fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(value) => serde_json::Value::String(value),
        toml::Value::Integer(value) => serde_json::Value::Number(value.into()),
        toml::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(value) => serde_json::Value::Bool(value),
        toml::Value::Datetime(value) => serde_json::Value::String(value.to_string()),
        toml::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_json(value)))
                .collect(),
        ),
    }
}

pub(crate) fn json_to_toml(value: &serde_json::Value) -> Result<String, ConvertError> {
    let object = value.as_object().ok_or(ConvertError::TomlNotTabular)?;
    Ok(toml::to_string_pretty(&json_object_to_toml(object))?)
}

pub(crate) fn json_object_to_toml(
    object: &serde_json::Map<String, serde_json::Value>,
) -> toml::Table {
    object
        .iter()
        .filter_map(|(key, value)| json_to_toml_value(value).map(|value| (key.to_owned(), value)))
        .collect()
}

pub(crate) fn json_to_toml_value(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(value) => Some(toml::Value::Boolean(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float)),
        serde_json::Value::String(value) => Some(toml::Value::String(value.to_owned())),
        serde_json::Value::Array(values) => Some(toml::Value::Array(
            values.iter().filter_map(json_to_toml_value).collect(),
        )),
        serde_json::Value::Object(object) => Some(toml::Value::Table(json_object_to_toml(object))),
    }
}

#[cfg(test)]
mod tests {

    use crate::convert::convert_bytes;
    use crate::format::OutputFormat;
    #[test]
    fn converts_csv_to_json_and_back() {
        let csv = b"name,age\nAda,36\nLin,28\n";
        let json = convert_bytes("people.csv", csv, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json[0].bytes).unwrap();
        assert_eq!(value[0]["name"], "Ada");
        assert_eq!(value[1]["age"], "28");

        let round_trip = convert_bytes("people.json", &json[0].bytes, OutputFormat::Csv).unwrap();
        let mut reader = csv::Reader::from_reader(round_trip[0].bytes.as_slice());
        let headers = reader.headers().unwrap().clone();
        let name_column = headers.iter().position(|header| header == "name").unwrap();
        let age_column = headers.iter().position(|header| header == "age").unwrap();
        let first = reader.records().next().unwrap().unwrap();
        assert_eq!(&first[name_column], "Ada");
        assert_eq!(&first[age_column], "36");
    }
    #[test]
    fn converts_between_json_yaml_and_toml() {
        let json = br#"{"name":"Ada","age":36,"tags":["math","code"]}"#;

        let yaml = convert_bytes("person.json", json, OutputFormat::Yaml).unwrap();
        let back = convert_bytes("person.yaml", &yaml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");

        let toml = convert_bytes("person.json", json, OutputFormat::Toml).unwrap();
        let back = convert_bytes("person.toml", &toml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");
    }
}
