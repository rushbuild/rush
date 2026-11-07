// Rush Build - Test Execution Engine
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Advanced test execution engine with sharding, parallelization, and smart selection.
//!
//! Provides comprehensive test running capabilities including test discovery,
//! sharding across workers, parallel execution, smart test selection based on
//! code changes, and detailed result reporting.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

/// Test execution engine.
pub struct TestRunner {
    /// Test execution config.
    config: TestConfig,

    /// Test result cache.
    result_cache: Arc<Mutex<HashMap<String, CachedTestResult>>>,

    /// Test coverage collector.
    coverage: Option<CoverageCollector>,

    /// Test sharding coordinator.
    sharding: ShardingCoordinator,
}

/// Test configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestConfig {
    /// Number of parallel test jobs.
    pub jobs: usize,

    /// Test timeout.
    pub timeout: Duration,

    /// Number of test shards.
    pub shards: usize,

    /// Current shard index (0-based).
    pub shard_index: usize,

    /// Test filter pattern.
    pub filter: Option<String>,

    /// Run only failed tests from last run.
    pub rerun_failed: bool,

    /// Collect code coverage.
    pub coverage: bool,

    /// Fail fast on first error.
    pub fail_fast: bool,

    /// Test execution strategy.
    pub strategy: TestStrategy,

    /// Verbose output.
    pub verbose: bool,

    /// Test repetition count.
    pub runs: usize,

    /// Shuffle test order.
    pub shuffle: bool,
}

/// Test execution strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TestStrategy {
    /// Run all tests.
    All,

    /// Only run tests affected by changes.
    Affected,

    /// Only run tests matching pattern.
    Pattern(String),

    /// Only run previously failed tests.
    FailedOnly,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig {
            jobs: num_cpus::get(),
            timeout: Duration::from_secs(300), // 5 minutes
            shards: 1,
            shard_index: 0,
            filter: None,
            rerun_failed: false,
            coverage: false,
            fail_fast: false,
            strategy: TestStrategy::All,
            verbose: false,
            runs: 1,
            shuffle: false,
        }
    }
}

/// Test target definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestTarget {
    /// Test label.
    pub label: String,

    /// Test executable path.
    pub executable: PathBuf,

    /// Test arguments.
    pub args: Vec<String>,

    /// Environment variables.
    pub env: HashMap<String, String>,

    /// Test size (small, medium, large, enormous).
    pub size: TestSize,

    /// Test timeout override.
    pub timeout: Option<Duration>,

    /// Test tags.
    pub tags: Vec<String>,

    /// Source files that affect this test.
    pub sources: Vec<PathBuf>,

    /// Dependencies.
    pub deps: Vec<String>,
}

/// Test size classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TestSize {
    /// Small test (< 1 minute).
    Small,

    /// Medium test (< 5 minutes).
    Medium,

    /// Large test (< 15 minutes).
    Large,

    /// Enormous test (> 15 minutes).
    Enormous,
}

impl TestSize {
    /// Get default timeout for test size.
    pub fn default_timeout(&self) -> Duration {
        match self {
            TestSize::Small => Duration::from_secs(60),
            TestSize::Medium => Duration::from_secs(300),
            TestSize::Large => Duration::from_secs(900),
            TestSize::Enormous => Duration::from_secs(3600),
        }
    }
}

/// Test result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    /// Test label.
    pub label: String,

    /// Test status.
    pub status: TestStatus,

    /// Execution duration.
    pub duration: Duration,

    /// Test output (stdout/stderr).
    pub output: String,

    /// Number of passed assertions.
    pub passed: usize,

    /// Number of failed assertions.
    pub failed: usize,

    /// Number of skipped tests.
    pub skipped: usize,

    /// Test shard index.
    pub shard: usize,

    /// Attempt number (for retries).
    pub attempt: usize,
}

/// Test status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TestStatus {
    /// Test passed.
    Passed,

    /// Test failed.
    Failed,

    /// Test was skipped.
    Skipped,

    /// Test timed out.
    Timeout,

    /// Test execution error.
    Error(String),
}

/// Cached test result.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedTestResult {
    /// Test result.
    result: TestResult,

    /// Source file hashes when test was run.
    source_hashes: HashMap<PathBuf, String>,

    /// Timestamp when cached.
    cached_at: std::time::SystemTime,
}

/// Test sharding coordinator.
struct ShardingCoordinator {
    /// Total number of shards.
    total_shards: usize,

    /// Current shard index.
    shard_index: usize,
}

impl ShardingCoordinator {
    fn new(total_shards: usize, shard_index: usize) -> Self {
        ShardingCoordinator {
            total_shards,
            shard_index,
        }
    }

    /// Assign tests to shards using consistent hashing.
    fn assign_shard(&self, test_label: &str) -> usize {
        let hash = self.hash_label(test_label);
        (hash % self.total_shards as u64) as usize
    }

    /// Check if test belongs to current shard.
    fn belongs_to_shard(&self, test_label: &str) -> bool {
        self.assign_shard(test_label) == self.shard_index
    }

    /// Hash test label for consistent sharding.
    fn hash_label(&self, label: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        label.hash(&mut hasher);
        hasher.finish()
    }

    /// Distribute tests across shards.
    fn _distribute_tests(&self, tests: &[TestTarget]) -> Vec<Vec<TestTarget>> {
        let mut shards = vec![Vec::new(); self.total_shards];

        for test in tests {
            let shard = self.assign_shard(&test.label);
            shards[shard].push(test.clone());
        }

        shards
    }
}

/// Coverage collector.
struct CoverageCollector {
    /// Coverage output directory.
    _output_dir: PathBuf,

    /// Coverage format.
    _format: CoverageFormat,
}

/// Coverage format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoverageFormat {
    /// LCOV format.
    Lcov,

    /// HTML report.
    Html,

    /// Cobertura XML.
    Cobertura,

    /// JSON format.
    Json,
}

impl TestRunner {
    /// Create new test runner.
    pub fn new(config: TestConfig) -> Result<Self> {
        let coverage = if config.coverage {
            Some(CoverageCollector {
                _output_dir: PathBuf::from("coverage"),
                _format: CoverageFormat::Lcov,
            })
        } else {
            None
        };

        let sharding = ShardingCoordinator::new(config.shards, config.shard_index);

        Ok(TestRunner {
            config,
            result_cache: Arc::new(Mutex::new(HashMap::new())),
            coverage,
            sharding,
        })
    }

    /// Run all tests.
    pub async fn run_tests(&self, tests: Vec<TestTarget>) -> Result<TestSummary> {
        let start = Instant::now();

        // Filter tests by strategy
        let filtered_tests = self.filter_tests(tests).await?;

        // Filter by shard
        let shard_tests: Vec<_> = filtered_tests
            .into_iter()
            .filter(|t| self.sharding.belongs_to_shard(&t.label))
            .collect();

        eprintln!(
            "Running {} tests (shard {}/{})",
            shard_tests.len(),
            self.config.shard_index + 1,
            self.config.shards
        );

        // Shuffle if requested
        let mut tests_to_run = shard_tests;
        if self.config.shuffle {
            use rand::seq::SliceRandom;
            let mut rng = rand::thread_rng();
            tests_to_run.shuffle(&mut rng);
        }

        // Execute tests in parallel
        let semaphore = Arc::new(Semaphore::new(self.config.jobs));
        let results = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for test in tests_to_run {
            let semaphore = Arc::clone(&semaphore);
            let results = Arc::clone(&results);
            let config = self.config.clone();
            let coverage = self.coverage.is_some();

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                for attempt in 0..config.runs {
                    let result = Self::execute_test(&test, &config, coverage, attempt).await;

                    results.lock().await.push(result.clone());

                    if config.fail_fast && result.status != TestStatus::Passed {
                        break;
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all tests
        for handle in handles {
            handle.await?;
        }

        let all_results = results.lock().await;

        // Generate summary
        let summary = TestSummary {
            total: all_results.len(),
            passed: all_results
                .iter()
                .filter(|r| r.status == TestStatus::Passed)
                .count(),
            failed: all_results
                .iter()
                .filter(|r| r.status == TestStatus::Failed)
                .count(),
            skipped: all_results
                .iter()
                .filter(|r| r.status == TestStatus::Skipped)
                .count(),
            timeout: all_results
                .iter()
                .filter(|r| r.status == TestStatus::Timeout)
                .count(),
            duration: start.elapsed(),
            results: all_results.clone(),
        };

        // Cache results
        self.cache_results(&summary.results).await?;

        Ok(summary)
    }

    /// Filter tests by strategy.
    async fn filter_tests(&self, tests: Vec<TestTarget>) -> Result<Vec<TestTarget>> {
        match &self.config.strategy {
            TestStrategy::All => Ok(tests),
            TestStrategy::Affected => self.filter_affected_tests(tests).await,
            TestStrategy::Pattern(pattern) => Ok(tests
                .into_iter()
                .filter(|t| t.label.contains(pattern))
                .collect()),
            TestStrategy::FailedOnly => self.filter_failed_tests(tests).await,
        }
    }

    /// Filter tests affected by recent changes.
    async fn filter_affected_tests(&self, tests: Vec<TestTarget>) -> Result<Vec<TestTarget>> {
        // TODO: Integrate with git to get changed files
        // For now, return all tests
        Ok(tests)
    }

    /// Filter previously failed tests.
    async fn filter_failed_tests(&self, tests: Vec<TestTarget>) -> Result<Vec<TestTarget>> {
        let cache = self.result_cache.lock().await;

        Ok(tests
            .into_iter()
            .filter(|t| {
                cache
                    .get(&t.label)
                    .map(|c| c.result.status != TestStatus::Passed)
                    .unwrap_or(true)
            })
            .collect())
    }

    /// Execute a single test.
    async fn execute_test(
        test: &TestTarget,
        config: &TestConfig,
        coverage: bool,
        attempt: usize,
    ) -> TestResult {
        let start = Instant::now();

        let timeout = test
            .timeout
            .unwrap_or_else(|| config.timeout.min(test.size.default_timeout()));

        // Build command
        let mut cmd = Command::new(&test.executable);
        cmd.args(&test.args);
        cmd.envs(&test.env);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if coverage {
            // Add coverage environment variables
            cmd.env("RUSH_COVERAGE", "1");
        }

        // Execute with timeout
        let result =
            tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || cmd.output())).await;

        let duration = start.elapsed();

        match result {
            Ok(Ok(Ok(output))) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined_output = format!("{}\n{}", stdout, stderr);

                let status = if output.status.success() {
                    TestStatus::Passed
                } else {
                    TestStatus::Failed
                };

                TestResult {
                    label: test.label.clone(),
                    status,
                    duration,
                    output: combined_output,
                    passed: 0, // Parse from test framework output
                    failed: 0,
                    skipped: 0,
                    shard: config.shard_index,
                    attempt,
                }
            }
            Ok(Ok(Err(e))) => TestResult {
                label: test.label.clone(),
                status: TestStatus::Error(e.to_string()),
                duration,
                output: String::new(),
                passed: 0,
                failed: 0,
                skipped: 0,
                shard: config.shard_index,
                attempt,
            },
            Ok(Err(_)) => TestResult {
                label: test.label.clone(),
                status: TestStatus::Error("Task join error".to_string()),
                duration,
                output: String::new(),
                passed: 0,
                failed: 0,
                skipped: 0,
                shard: config.shard_index,
                attempt,
            },
            Err(_) => TestResult {
                label: test.label.clone(),
                status: TestStatus::Timeout,
                duration,
                output: String::new(),
                passed: 0,
                failed: 0,
                skipped: 0,
                shard: config.shard_index,
                attempt,
            },
        }
    }

    /// Cache test results.
    async fn cache_results(&self, results: &[TestResult]) -> Result<()> {
        let mut cache = self.result_cache.lock().await;

        for result in results {
            let cached = CachedTestResult {
                result: result.clone(),
                source_hashes: HashMap::new(), // TODO: Compute hashes
                cached_at: std::time::SystemTime::now(),
            };

            cache.insert(result.label.clone(), cached);
        }

        Ok(())
    }

    /// Load cached results from disk.
    pub async fn load_cache(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(path).await?;
        let cache: HashMap<String, CachedTestResult> = serde_json::from_str(&content)?;

        *self.result_cache.lock().await = cache;

        Ok(())
    }

    /// Save cached results to disk.
    pub async fn save_cache(&self, path: &Path) -> Result<()> {
        let cache = self.result_cache.lock().await;
        let content = serde_json::to_string_pretty(&*cache)?;

        tokio::fs::write(path, content).await?;

        Ok(())
    }
}

/// Test execution summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSummary {
    /// Total tests run.
    pub total: usize,

    /// Passed tests.
    pub passed: usize,

    /// Failed tests.
    pub failed: usize,

    /// Skipped tests.
    pub skipped: usize,

    /// Timed out tests.
    pub timeout: usize,

    /// Total duration.
    pub duration: Duration,

    /// Individual results.
    pub results: Vec<TestResult>,
}

impl TestSummary {
    /// Print summary to console.
    pub fn print(&self) {
        eprintln!("\n{}", "=".repeat(70));
        eprintln!("Test Results");
        eprintln!("{}", "=".repeat(70));
        eprintln!("Total:   {}", self.total);
        eprintln!("Passed:  {} ✓", self.passed);
        eprintln!("Failed:  {} ✗", self.failed);
        eprintln!("Skipped: {}", self.skipped);
        eprintln!("Timeout: {}", self.timeout);
        eprintln!("Duration: {:.2}s", self.duration.as_secs_f64());
        eprintln!("{}", "=".repeat(70));

        if self.failed > 0 {
            eprintln!("\nFailed tests:");
            for result in &self.results {
                if result.status == TestStatus::Failed {
                    eprintln!("  - {}", result.label);
                }
            }
        }
    }

    /// Check if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.timeout == 0
    }

    /// Export to JUnit XML format.
    pub fn to_junit_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str(&format!(
            "<testsuite tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
            self.total,
            self.failed + self.timeout,
            self.skipped,
            self.duration.as_secs_f64()
        ));

        for result in &self.results {
            xml.push_str(&format!(
                "  <testcase name=\"{}\" time=\"{:.3}\"",
                result.label,
                result.duration.as_secs_f64()
            ));

            match &result.status {
                TestStatus::Passed => {
                    xml.push_str(" />\n");
                }
                TestStatus::Failed => {
                    xml.push_str(">\n");
                    xml.push_str("    <failure message=\"Test failed\">\n");
                    xml.push_str(&format!("      <![CDATA[{}]]>\n", result.output));
                    xml.push_str("    </failure>\n");
                    xml.push_str("  </testcase>\n");
                }
                TestStatus::Skipped => {
                    xml.push_str(">\n");
                    xml.push_str("    <skipped />\n");
                    xml.push_str("  </testcase>\n");
                }
                TestStatus::Timeout => {
                    xml.push_str(">\n");
                    xml.push_str("    <failure message=\"Test timeout\" />\n");
                    xml.push_str("  </testcase>\n");
                }
                TestStatus::Error(msg) => {
                    xml.push_str(">\n");
                    xml.push_str(&format!("    <error message=\"{}\" />\n", msg));
                    xml.push_str("  </testcase>\n");
                }
            }
        }

        xml.push_str("</testsuite>\n");
        xml
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sharding() {
        let coordinator = ShardingCoordinator::new(4, 0);

        // Same label always goes to same shard
        let shard1 = coordinator.assign_shard("test1");
        let shard2 = coordinator.assign_shard("test1");
        assert_eq!(shard1, shard2);

        // Belongs to shard check
        assert_eq!(coordinator.belongs_to_shard("test1"), shard1 == 0);
    }

    #[test]
    fn test_test_size_timeout() {
        assert_eq!(TestSize::Small.default_timeout(), Duration::from_secs(60));
        assert_eq!(TestSize::Large.default_timeout(), Duration::from_secs(900));
    }
}
