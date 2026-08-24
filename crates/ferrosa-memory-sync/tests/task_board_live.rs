//! Reads the REAL board, because a query that compiles is not a query that
//! returns rows.
//!
//! The schema is forge's, in another repository, and the attribution field a
//! task uses depends on which tool captured it. Nothing about that is checked
//! by the type system.

use ferrosa_memory_sync::task_board::TaskBoard;

fn contact_points() -> Vec<String> {
    vec![
        "127.0.0.1:19042".to_owned(),
        "127.0.0.1:19043".to_owned(),
        "127.0.0.1:19044".to_owned(),
    ]
}

#[tokio::test]
#[ignore = "needs the live task board"]
async fn the_board_returns_this_repositorys_work() {
    let board = TaskBoard::connect(&contact_points())
        .await
        .expect("connects to the board");

    let mine = board
        .open_work(&["/Users/bkearns/src/ferrosa-suite/ferrosa-mobile".to_owned()])
        .await
        .expect("reads the board");

    println!("found {} open tasks for ferrosa-mobile", mine.len());
    for task in mine.iter().take(10) {
        println!(
            "  [{}] p{} {} {}{}",
            task.status,
            task.priority,
            task.id,
            task.title,
            if task.waits_on_a_person() {
                "  <- needs a person"
            } else {
                ""
            }
        );
    }

    assert!(
        !mine.is_empty(),
        "the board has open ferrosa-mobile tasks captured earlier today; an empty \
         result means the query or the attribution rule is wrong, not that there \
         is no work"
    );
}

/// A directory nothing was ever filed against returns nothing, rather than the
/// whole board. Proves the filter filters.
#[tokio::test]
#[ignore = "needs the live task board"]
async fn an_unrelated_directory_gets_nothing() {
    let board = TaskBoard::connect(&contact_points())
        .await
        .expect("connects to the board");
    let none = board
        .open_work(&["/tmp/definitely-not-a-repo".to_owned()])
        .await
        .expect("reads the board");
    assert!(
        none.is_empty(),
        "the filter let {} tasks through",
        none.len()
    );
}

/// A task can be read in full, with its body.
///
/// The list carries titles; the detail screen needs the prose, and the prose is
/// the whole reason to open one. Read against the real board because the column
/// set is forge's, in another repository.
#[tokio::test]
#[ignore = "needs the live task board"]
async fn one_task_can_be_read_in_full() {
    let board = TaskBoard::connect(&contact_points())
        .await
        .expect("connects to the board");

    let listed = board
        .open_work(&["/Users/bkearns/src/ferrosa-suite/ferrosa-mobile".to_owned()])
        .await
        .expect("reads the board");
    let first = listed.first().expect("the board has open work");

    let detail = board
        .detail(&first.id)
        .await
        .expect("reads one task")
        .expect("the task listed a moment ago is still there");

    println!("{} — {}", detail.task.id, detail.task.title);
    println!(
        "body is {} chars, {} comment(s)",
        detail.body.len(),
        detail.comments.len()
    );

    assert_eq!(detail.task.id, first.id);
    assert!(
        !detail.body.is_empty(),
        "the body is the reason to open a task; an empty one means the column \
         is not being read"
    );
}

/// An id that was never on the board is absent, not an error.
#[tokio::test]
#[ignore = "needs the live task board"]
async fn an_unknown_task_is_absent_rather_than_an_error() {
    let board = TaskBoard::connect(&contact_points())
        .await
        .expect("connects to the board");
    let missing = board
        .detail("t_definitely_not_real")
        .await
        .expect("a missing task is not a failure");
    assert!(missing.is_none());
}
