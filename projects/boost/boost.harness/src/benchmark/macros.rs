/// Declares one or more benchmark functions that share a suite entry point.
///
/// ```ignore
/// fn bench_example(c: &mut BenchmarkSuite) { /* ... */ }
/// benchmark_group!(benches, bench_example);
/// benchmark_main!(benches);
/// ```
#[macro_export]
macro_rules! benchmark_group {
    ($name:ident, $($function:ident),*) => {
        fn $name(c: &mut $crate::benchmark::benchmark::BenchmarkSuite) {
            $($function(c);)*
        }
    };
}

/// Runs declared benchmark groups and prints structured reports.
#[macro_export]
macro_rules! benchmark_main {
    ($($group:ident),*) => {
        fn main() {
            let mut suite = $crate::benchmark::benchmark::BenchmarkSuite::new();
            $($group(&mut suite);)*
            suite.finish();
        }
    };
}
