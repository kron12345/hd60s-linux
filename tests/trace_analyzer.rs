use hd60s_linux::trace::analyze_tsv;

#[test]
fn synthetic_trace_matches_sanitized_and_summary_goldens() {
    let input = include_str!("fixtures/synthetic-trace.tsv");
    let expected_sanitized = include_str!("fixtures/synthetic-trace.sanitized.tsv");
    let expected_summary = include_str!("fixtures/synthetic-trace.summary.txt");

    let analysis = analyze_tsv(input).expect("synthetic trace should analyze");
    assert_eq!(analysis.sanitized_tsv, expected_sanitized);
    assert_eq!(analysis.summary.render(), expected_summary);
}
