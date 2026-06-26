# ferrumc-command

A Brigadier-lite command system: a tree of literal and argument nodes with
parsing, tab-completion suggestions, and dispatch that delegates the actual
permission check to the calling layer.

Depends only on `ferrumc-core`. It does not know how permissions are stored or
evaluated: a node declares a required permission *level* and/or an opaque
permission *node string*, and the caller supplies the check — a permission level
on the `CommandSource` (compared during `dispatch`), or an `is_allowed` closure
passed to `dispatch_with`.

## Example

```rust
use ferrumc_command::{argument, literal, ArgumentType, CommandResult, CommandSource, CommandTree};
use ferrumc_core::TextComponent;

let mut tree = CommandTree::new();
tree.register(
    literal("gamemode")
        .requires_level(2)
        .then(argument("mode", ArgumentType::integer(0, 3)).executes(|ctx| {
            let mode = ctx.integer("mode").unwrap_or_default();
            CommandResult::success(TextComponent::text(format!("gamemode set to {mode}")))
        })),
);

let op = CommandSource::console(4);
let result = tree.dispatch("gamemode 1", &op);
assert!(matches!(&result, Ok(outcome) if outcome.is_success()));

// Suggestions complete the next token.
assert_eq!(tree.suggest("game"), vec!["gamemode".to_string()]);
```

## Invariants

See `INVARIANTS.md` in this directory.
