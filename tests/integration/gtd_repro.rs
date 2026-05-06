use crate::common::create_psr4_workspace;
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

#[tokio::test]
async fn test_gtd_broken_repro() {
    let (backend, dir) = create_psr4_workspace(
        r#"{ "autoload": { "psr-4": { "App\\": "src/", "Illuminate\\": "vendor/illuminate/" } } }"#,
        &[
            (
                "vendor/illuminate/Model.php",
                "<?php namespace Illuminate\\Database\\Eloquent; abstract class Model {
                public static function query() {}
            }",
            ),
            (
                "vendor/illuminate/Builder.php",
                "<?php namespace Illuminate\\Database\\Eloquent; class Builder {
                /** @return $this */
                public function where($c, $v = null) { return $this; }
                /** @return $this */
                public function orWhereBetween($c, array $v) { return $this; }
            }",
            ),
            (
                "src/User.php",
                "<?php namespace App; use Illuminate\\Database\\Eloquent\\Model; class User extends Model {}",
            ),
        ],
    );

    // Ensure all files are indexed
    for path in [
        "vendor/illuminate/Builder.php",
        "vendor/illuminate/Model.php",
        "src/User.php",
    ] {
        let uri = Url::from_file_path(dir.path().join(path)).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "php".to_string(),
                    version: 1,
                    text: std::fs::read_to_string(dir.path().join(path)).unwrap(),
                },
            })
            .await;
    }

    let uri = Url::from_file_path(dir.path().join("test.php")).unwrap();
    //                   0123456789012345678901234567890123456789012345
    let content = "<?php use App\\User; User::orWhereBetween('id', [1, 2]);";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "php".to_string(),
                version: 1,
                text: content.to_string(),
            },
        })
        .await;

    // Test GTD for orWhereBetween
    let req = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            position: Position::new(0, 30), // On 'orWhereBetween'
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let resp = backend.goto_definition(req).await.unwrap();
    assert!(resp.is_some(), "GTD for orWhereBetween failed");
    let locs = match resp.unwrap() {
        GotoDefinitionResponse::Scalar(l) => vec![l],
        GotoDefinitionResponse::Array(a) => a,
        _ => panic!("Expected locations"),
    };
    assert!(
        !locs.is_empty(),
        "GTD for orWhereBetween returned empty list"
    );
    let target_uri = locs[0].uri.to_string();
    assert!(
        target_uri.contains("Builder.php"),
        "Should point to Builder.php, got {}",
        target_uri
    );
}
