use nu_test_support::fs::Stub::FileWithContentToBeTrimmed;
use nu_test_support::playground::Playground;
use nu_test_support::{nu, pipeline};

#[test]
fn table_to_csv_text_and_from_csv_text_back_into_table() {
    let actual = nu!(
        cwd: "tests/fixtures/formats",
        "open caco3_plastics.csv | to csv | from csv | first | get origin "
    );

    assert_eq!(actual.out, "SPAIN");
}

#[test]
fn table_to_csv_text() {
    Playground::setup("filter_to_csv_test_1", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "csv_text_sample.txt",
            r#"
                importer,shipper,tariff_item,name,origin
                Plasticos Rival,Reverte,2509000000,Calcium carbonate,Spain
                Tigre Ecuador,OMYA Andina,3824909999,Calcium carbonate,Colombia
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open csv_text_sample.txt
                | lines
                | str trim
                | split column "," a b c d origin
                | last 1
                | to csv
                | lines
                | get 1
            "#
        ));

        assert!(actual
            .out
            .contains("Tigre Ecuador,OMYA Andina,3824909999,Calcium carbonate,Colombia"));
    })
}

#[test]
fn table_to_csv_text_skipping_headers_after_conversion() {
    Playground::setup("filter_to_csv_test_2", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "csv_text_sample.txt",
            r#"
                importer,shipper,tariff_item,name,origin
                Plasticos Rival,Reverte,2509000000,Calcium carbonate,Spain
                Tigre Ecuador,OMYA Andina,3824909999,Calcium carbonate,Colombia
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open csv_text_sample.txt
                | lines
                | str trim
                | split column "," a b c d origin
                | last 1
                | to csv --noheaders
            "#
        ));

        assert!(actual
            .out
            .contains("Tigre Ecuador,OMYA Andina,3824909999,Calcium carbonate,Colombia"));
    })
}

#[test]
fn infers_types() {
    Playground::setup("filter_from_csv_test_1", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_cuatro_mosqueteros.csv",
            r#"
                first_name,last_name,rusty_luck,d
                Andrés,Robalino,1,d
                Jonathan,Turner,1,d
                Yehuda,Katz,1,d
                Jason,Gedge,1,d
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_cuatro_mosqueteros.csv
                | where rusty_luck > 0
                | length
            "#
        ));

        assert_eq!(actual.out, "4");
    })
}

#[test]
fn from_csv_text_to_table() {
    Playground::setup("filter_from_csv_test_2", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                Andrés,Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv
                | get rusty_luck
                | length
            "#
        ));

        assert_eq!(actual.out, "3");
    })
}

#[test]
fn from_csv_text_with_separator_to_table() {
    Playground::setup("filter_from_csv_test_3", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name;last_name;rusty_luck
                Andrés;Robalino;1
                Jonathan;Turner;1
                Yehuda;Katz;1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --separator ";"
                | get rusty_luck
                | length
            "#
        ));

        assert_eq!(actual.out, "3");
    })
}

#[test]
fn from_csv_text_with_tab_separator_to_table() {
    Playground::setup("filter_from_csv_test_4", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name	last_name	rusty_luck
                Andrés	Robalino	1
                Jonathan	Turner	1
                Yehuda	Katz	1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --separator (char tab)
                | get rusty_luck
                | length
            "#
        ));

        assert_eq!(actual.out, "3");
    })
}

#[test]
fn from_csv_text_with_comments_to_table() {
    Playground::setup("filter_from_csv_test_5", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                # This is a comment
                first_name,last_name,rusty_luck
                # This one too
                Andrés,Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
                # This one also
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r##"
                open los_tres_caballeros.txt
                | from csv --comment "#"
                | get rusty_luck
                | length
            "##
        ));

        assert_eq!(actual.out, "3");
    })
}

#[test]
fn from_csv_text_with_custom_quotes_to_table() {
    Playground::setup("filter_from_csv_test_6", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                'And''rés',Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --quote "'"
                | first
                | get first_name
            "#
        ));

        assert_eq!(actual.out, "And'rés");
    })
}

#[test]
fn from_csv_text_with_custom_escapes_to_table() {
    Playground::setup("filter_from_csv_test_7", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                "And\"rés",Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --escape '\'
                | first
                | get first_name
            "#
        ));

        assert_eq!(actual.out, "And\"rés");
    })
}

#[test]
fn from_csv_text_skipping_headers_to_table() {
    Playground::setup("filter_from_csv_test_8", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_amigos.txt",
            r#"
                Andrés,Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_amigos.txt
                | from csv --noheaders
                | get column3
                | length
            "#
        ));

        assert_eq!(actual.out, "3");
    })
}

#[test]
fn from_csv_text_with_missing_columns_to_table() {
    Playground::setup("filter_from_csv_test_9", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                Andrés,Robalino
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --flexible
                | get -i rusty_luck
                | compact
                | length
            "#
        ));

        assert_eq!(actual.out, "2");
    })
}

#[test]
fn from_csv_text_with_multiple_char_separator() {
    Playground::setup("filter_from_csv_test_10", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                Andrés,Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --separator "li"
            "#
        ));

        assert!(actual.err.contains("single character separator"));
    })
}

#[test]
fn from_csv_text_with_wrong_type_separator() {
    Playground::setup("filter_from_csv_test_11", |dirs, sandbox| {
        sandbox.with_files(vec![FileWithContentToBeTrimmed(
            "los_tres_caballeros.txt",
            r#"
                first_name,last_name,rusty_luck
                Andrés,Robalino,1
                Jonathan,Turner,1
                Yehuda,Katz,1
            "#,
        )]);

        let actual = nu!(
            cwd: dirs.test(), pipeline(
            r#"
                open los_tres_caballeros.txt
                | from csv --separator ('123' | into int)
            "#
        ));

        assert!(actual.err.contains("can't convert int to char"));
    })
}

#[test]
fn table_with_record_error() {
    let actual = nu!(
        cwd: "tests/fixtures/formats", pipeline(
        r#"
            [[a b]; [1 2] [3 {a: 1 b: 2}]] 
            | to csv
        "#
    ));

    assert!(actual.err.contains("can't convert"))
}

#[test]
fn list_not_table_error() {
    let actual = nu!(
        cwd: "tests/fixtures/formats", pipeline(
        r#"
            [{a: 1 b: 2} {a: 3 b: 4} 1]
            | to csv
        "#
    ));

    assert!(actual.err.contains("can't convert"))
}

#[test]
fn string_to_csv_error() {
    let actual = nu!(
        cwd: "tests/fixtures/formats", pipeline(
        r#"
            'qwe' | to csv
        "#
    ));

    assert!(actual.err.contains("can't convert"))
}
