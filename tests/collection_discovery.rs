use dbeel::{
    args::{parse_args_from, Args},
    error::Result,
};
use dbeel_client::create_collection;
use rstest::{fixture, rstest};
use test_utils::test_shard_with_args;

#[fixture]
fn args() -> Args {
    // Remove the test directory if it exists.
    let _ = std::fs::remove_dir("/tmp/test");
    parse_args_from(["", "--dir", "/tmp/test"])
}

#[rstest]
fn clean_state(args: Args) -> Result<()> {
    test_shard_with_args(args, |shard| async move {
        assert!(shard.trees.borrow().is_empty());
    })
}

#[rstest]
fn find_collections_after_rerun(args: Args) -> Result<()> {
    test_shard_with_args(args.clone(), |shard| async move {
        create_collection("test", &(shard.args.ip.clone(), shard.args.port))
            .await
            .unwrap();

        assert_eq!(shard.trees.borrow().len(), 1);
        assert!(shard.trees.borrow().get(&"test".to_string()).is_some());
    })?;

    test_shard_with_args(args.clone(), |shard| async move {
        assert_eq!(shard.trees.borrow().len(), 1);
        assert!(shard.trees.borrow().get(&"test".to_string()).is_some());
    })?;

    Ok(())
}
