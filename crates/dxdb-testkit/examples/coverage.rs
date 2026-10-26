fn main() {
    let report = dxdb_testkit::report::Report::compute();
    print!("{}", report.to_markdown());
    println!("### Unexercised cases, by reason\n");
    for (status, cases) in report.unexercised() {
        println!("**{}** ({} cases)", status, cases.len());
        for case in cases {
            println!("- `{}` {} — {}", case.id, case.citation(), case.title);
        }
        println!();
    }
}
