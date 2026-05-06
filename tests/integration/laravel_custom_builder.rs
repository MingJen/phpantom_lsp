use crate::common::create_psr4_workspace;
use tower_lsp::LanguageServer;
use tower_lsp::lsp_types::*;

#[tokio::test]
async fn test_custom_eloquent_builder_attribute() {
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
                "src/Models/UserBuilder.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Builder;
class UserBuilder extends Builder {
    /** @return $this */
    public function active() { return $this; }
}
"#,
            ),
            (
                "src/Models/User.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
use Illuminate\Database\Eloquent\Attributes\UseEloquentBuilder;
#[UseEloquentBuilder(UserBuilder::class)]
class User extends Model {}
"#,
            ),
        ],
    );

    // Ensure all files are indexed
    for path in [
        "vendor/illuminate/Builder.php",
        "vendor/illuminate/Model.php",
        "src/Models/UserBuilder.php",
        "src/Models/User.php",
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
    let content = "<?php\nuse App\\Models\\User;\nUser::act";
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

    // Test static call: User::active()
    let req = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            position: Position::new(2, 9),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = backend.completion(req).await.unwrap().unwrap();
    let labels: Vec<_> = match items {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };
    assert!(
        labels.iter().any(|l| l.starts_with("active")),
        "Should suggest custom builder method. Labels: {:?}",
        labels
    );

    // Test query() return type
    let uri2 = Url::from_file_path(dir.path().join("test2.php")).unwrap();
    let content2 = "<?php\nuse App\\Models\\User;\nUser::query()->act";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri2.clone(),
                language_id: "php".to_string(),
                version: 1,
                text: content2.to_string(),
            },
        })
        .await;

    let req = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri2.clone()),
            position: Position::new(2, 18),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = backend.completion(req).await.unwrap().unwrap();
    let labels: Vec<_> = match items {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };
    assert!(
        labels.iter().any(|l| l.starts_with("active")),
        "query() should return custom builder. Labels: {:?}",
        labels
    );

    // Test orWhereBetween on model (forwarded from base Builder via custom builder)
    let uri3 = Url::from_file_path(dir.path().join("test3.php")).unwrap();
    let content3 = "<?php\nuse App\\Models\\User;\nUser::orWhereBet";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri3.clone(),
                language_id: "php".to_string(),
                version: 1,
                text: content3.to_string(),
            },
        })
        .await;

    let req = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri3.clone()),
            position: Position::new(2, 16),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = backend.completion(req).await.unwrap().unwrap();
    let labels: Vec<_> = match items {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };
    assert!(
        labels.iter().any(|l| l.starts_with("orWhereBetween")),
        "Should suggest forwarded builder method. Labels: {:?}",
        labels
    );
}

#[tokio::test]
async fn test_goto_definition_relationship_string() {
    let (backend, dir) = create_psr4_workspace(
        r#"{ "autoload": { "psr-4": { "App\\": "src/", "Illuminate\\": "vendor/illuminate/" } } }"#,
        &[
            (
                "vendor/illuminate/Model.php",
                "<?php namespace Illuminate\\Database\\Eloquent; abstract class Model {
                public static function with($r) {}
                public function hasMany($c) {}
            }",
            ),
            (
                "vendor/illuminate/Relations/HasMany.php",
                "<?php namespace Illuminate\\Database\\Eloquent\\Relations; class HasMany {}",
            ),
            (
                "src/Models/Post.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class Post extends Model {
    /** @return \Illuminate\Database\Eloquent\Relations\HasMany<Comment, $this> */
    pub function comments() { return $this->hasMany(Comment::class); }
}
"#,
            ),
            (
                "src/Models/Comment.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class Comment extends Model {}
"#,
            ),
        ],
    );

    // Explicitly open the model file to ensure it's in the ast_map
    for path in ["vendor/illuminate/Model.php", "src/Models/Post.php"] {
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
    let content = "<?php\nuse App\\Models\\Post;\nPost::with('comments');";
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

    // Test GTD in with('comments')
    let req = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(2, 15),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let resp = backend.goto_definition(req).await.unwrap();
    let locs = match resp {
        Some(GotoDefinitionResponse::Scalar(l)) => vec![l],
        Some(GotoDefinitionResponse::Array(a)) => a,
        None => panic!("Should have found a location for 'comments'"),
        _ => panic!("Expected locations"),
    };

    assert!(!locs.is_empty(), "Should resolve comments string to method");
    let uri_res = locs[0].uri.to_string();
    assert!(
        uri_res.contains("Post.php"),
        "Should point to Post.php, got {}",
        uri_res
    );
    // comments() is on line 6 (0-indexed 5)
    assert_eq!(locs[0].range.start.line, 5);
}

#[tokio::test]
async fn test_goto_definition_forwarded_builder_method() {
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
                public function orWhereBetween($c, array $v) { return $this; }
            }",
            ),
            (
                "src/Models/User.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class User extends Model {}
"#,
            ),
        ],
    );

    // Open Builder.php and User.php
    for path in [
        "vendor/illuminate/Builder.php",
        "vendor/illuminate/Model.php",
        "src/Models/User.php",
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
    let content = "<?php\nuse App\\Models\\User;\nUser::orWhereBetween('id', [1, 2]);";
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

    // Test GTD on orWhereBetween
    let req = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(2, 10), // On 'orWhereBetween'
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let resp = backend.goto_definition(req).await.unwrap();
    let locs = match resp {
        Some(GotoDefinitionResponse::Scalar(l)) => vec![l],
        Some(GotoDefinitionResponse::Array(a)) => a,
        None => panic!("Should have found a location for 'orWhereBetween'"),
        _ => panic!("Expected locations"),
    };

    assert!(
        !locs.is_empty(),
        "Should resolve orWhereBetween to Builder method"
    );
    let uri_res = locs[0].uri.to_string();
    assert!(
        uri_res.contains("Builder.php"),
        "Should point to Builder.php, got {}",
        uri_res
    );
    // orWhereBetween is on line 3 (0-indexed 2)
    assert_eq!(locs[0].range.start.line, 2);
}

#[tokio::test]
async fn test_completion_relationship_string() {
    let (backend, dir) = create_psr4_workspace(
        r#"{ "autoload": { "psr-4": { "App\\": "src/", "Illuminate\\": "vendor/illuminate/" } } }"#,
        &[
            (
                "vendor/illuminate/Model.php",
                "<?php namespace Illuminate\\Database\\Eloquent; abstract class Model {
                public static function with($r) {}
                public function hasMany($c) {}
            }",
            ),
            (
                "vendor/illuminate/Relations/HasMany.php",
                "<?php namespace Illuminate\\Database\\Eloquent\\Relations; class HasMany {}",
            ),
            (
                "src/Models/Post.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class Post extends Model {
    /** @return \Illuminate\Database\Eloquent\Relations\HasMany<Comment, $this> */
    pub function comments() { return $this->hasMany(Comment::class); }
    /** @return \Illuminate\Database\Eloquent\Relations\HasMany<Author, $this> */
    pub function authors() { return $this->hasMany(Author::class); }
}
"#,
            ),
            (
                "src/Models/Comment.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class Comment extends Model {
    /** @return \Illuminate\Database\Eloquent\Relations\BelongsTo<Author, $this> */
    pub function author() { return $this->belongsTo(Author::class); }
}
"#,
            ),
            (
                "src/Models/Author.php",
                "<?php namespace App\\Models; class Author extends \\Illuminate\\Database\\Eloquent\\Model {}",
            ),
        ],
    );

    for path in ["vendor/illuminate/Model.php", "src/Models/Post.php"] {
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
    let content = "<?php\nuse App\\Models\\Post;\nPost::with('c');";
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

    let req = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri.clone()),
            position: Position::new(2, 13), // inside with('c|)
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = backend.completion(req).await.unwrap().unwrap();
    let labels: Vec<_> = match items {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };

    assert!(
        labels.contains(&"comments".to_string()),
        "Should suggest 'comments'. Labels: {:?}",
        labels
    );
    assert!(
        !labels.contains(&"authors".to_string()),
        "Should NOT suggest 'authors' when partial is 'c'. Labels: {:?}",
        labels
    );

    // Test dot-notation: Post::with('comments.a')
    let content2 = "<?php\nuse App\\Models\\Post;\nPost::with('comments.a');";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "php".to_string(),
                version: 2,
                text: content2.to_string(),
            },
        })
        .await;

    let req2 = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(2, 22), // inside with('comments.a|)
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items2 = backend.completion(req2).await.unwrap().unwrap();
    let labels2: Vec<_> = match items2 {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };

    assert!(
        labels2.contains(&"author".to_string()),
        "Should suggest 'author' from Comment model. Labels: {:?}",
        labels2
    );
}

#[tokio::test]
async fn test_completion_relationship_string_after_builder_receiver() {
    let (backend, dir) = create_psr4_workspace(
        r#"{ "autoload": { "psr-4": { "App\\": "src/", "Illuminate\\": "vendor/illuminate/" } } }"#,
        &[
            (
                "vendor/illuminate/Model.php",
                "<?php namespace Illuminate\\Database\\Eloquent; abstract class Model {
                /** @return Builder<static> */
                public static function query() {}
                public function hasMany($c) {}
            }",
            ),
            (
                "vendor/illuminate/Builder.php",
                "<?php namespace Illuminate\\Database\\Eloquent;
                /** @template TModel */
                class Builder {
                    /** @return $this */
                    public function with($r) { return $this; }
                }",
            ),
            (
                "vendor/illuminate/Relations/HasMany.php",
                "<?php namespace Illuminate\\Database\\Eloquent\\Relations; class HasMany {}",
            ),
            (
                "src/Models/Post.php",
                r#"<?php
namespace App\Models;
use Illuminate\Database\Eloquent\Model;
class Post extends Model {
    /** @return \Illuminate\Database\Eloquent\Relations\HasMany<Comment, $this> */
    public function comments() { return $this->hasMany(Comment::class); }
}
"#,
            ),
            (
                "src/Models/Comment.php",
                "<?php namespace App\\Models; class Comment extends \\Illuminate\\Database\\Eloquent\\Model {}",
            ),
        ],
    );

    for path in [
        "vendor/illuminate/Builder.php",
        "vendor/illuminate/Model.php",
        "src/Models/Post.php",
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
    let content = "<?php\nuse App\\Models\\Post;\nPost::query()->with('c');";
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

    let req = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier::new(uri),
            position: Position::new(2, 22),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = backend.completion(req).await.unwrap().unwrap();
    let labels: Vec<_> = match items {
        CompletionResponse::Array(arr) => arr.into_iter().map(|i| i.label).collect(),
        _ => panic!("Expected array"),
    };

    assert!(
        labels.contains(&"comments".to_string()),
        "Builder<Post>::with() should suggest Post relationships. Labels: {:?}",
        labels
    );
}
