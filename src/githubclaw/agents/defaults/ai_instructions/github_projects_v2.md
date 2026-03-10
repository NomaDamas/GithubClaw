# GitHub Projects V2

## Querying
Use the GraphQL API via `gh api graphql` to query project board items,
statuses, and custom fields.

## Updating
- Move items between columns by updating the Status field.
- Add new items with `addProjectV2ItemById`.
- Always verify the current state before making changes.
