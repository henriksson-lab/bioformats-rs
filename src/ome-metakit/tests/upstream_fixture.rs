use ome_metakit::{ColumnType, MetakitReader, Value};

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("test.mk")
}

#[test]
fn upstream_fixture_table_metadata_is_consistent() {
    let reader = MetakitReader::open(fixture()).unwrap();
    assert_eq!(reader.table_count(), reader.table_names().len());
    assert!(reader.table_count() > 0);

    for table_index in 0..reader.table_count() {
        let table = reader.table(table_index).unwrap();
        assert!(!table.name().is_empty());
        assert_eq!(
            table.columns().len(),
            reader.column_names(table_index).unwrap().len()
        );
        assert_eq!(
            table.columns().len(),
            reader.column_types(table_index).unwrap().len()
        );
        assert_eq!(table.row_count(), reader.row_count(table_index).unwrap());
    }
}

#[test]
fn upstream_fixture_rows_match_bulk_table_data() {
    let reader = MetakitReader::open(fixture()).unwrap();

    for table_index in 0..reader.table_count() {
        let rows = reader.table_data(table_index).unwrap();
        for (row_index, row) in rows.iter().enumerate() {
            assert_eq!(*row, reader.row_data(row_index, table_index).unwrap());
        }
    }
}

#[test]
fn upstream_fixture_values_match_declared_column_types() {
    let reader = MetakitReader::open(fixture()).unwrap();

    for table in reader.tables() {
        for row in table.rows() {
            for (column, value) in table.columns().iter().zip(row) {
                let Some(value) = value else {
                    continue;
                };
                match (column.column_type(), value) {
                    (ColumnType::String, Value::String(_)) => {}
                    (ColumnType::Integer, Value::Integer(_)) => {}
                    (ColumnType::Float, Value::Float(_)) => {}
                    (ColumnType::Double, Value::Double(_)) => {}
                    (ColumnType::Bytes, Value::Bytes(_)) => {}
                    (ColumnType::Long, Value::Long(_)) => {}
                    (expected, actual) => {
                        panic!("value {actual:?} does not match column type {expected:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn upstream_fixture_matches_known_table_shape() {
    let reader = MetakitReader::open(fixture()).unwrap();
    assert_eq!(
        reader.table_names(),
        vec![
            "variablesView",
            "samplesViewR",
            "stringsViewR",
            "filesViewR"
        ]
    );
    assert_eq!(reader.row_count_by_table("variablesView"), Some(1));
    assert_eq!(reader.row_count_by_table("samplesViewR"), Some(29));
    assert_eq!(reader.row_count_by_table("stringsViewR"), Some(23));
    assert_eq!(reader.row_count_by_table("filesViewR"), Some(0));

    let variables = reader.table_by_name("variablesView").unwrap();
    assert_eq!(
        variables.column_names(),
        vec![
            "varVersion",
            "varNextSampleID",
            "varNextStringID",
            "varNextFileID",
            "varDemoKey"
        ]
    );
    assert_eq!(
        variables.row(0).unwrap(),
        [
            Some(Value::Integer(2)),
            Some(Value::Integer(30)),
            Some(Value::Integer(24)),
            Some(Value::Integer(2)),
            None
        ]
    );

    let strings = reader.table_by_name("stringsViewR").unwrap();
    assert_eq!(
        strings.row(0).unwrap(),
        [
            Some(Value::Integer(1)),
            Some(Value::String("clipping-test.mvd2\0".to_owned())),
            Some(Value::Integer(1))
        ]
    );
}
