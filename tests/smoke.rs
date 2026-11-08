mod common;

mod cli {
    mod version {
        use crate::common::TestEnv;

        #[test]
        fn prints_version() {
            let env = TestEnv::new();
            env.cmd()
                .arg("--version")
                .assert()
                .success()
                .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
        }
    }
}
