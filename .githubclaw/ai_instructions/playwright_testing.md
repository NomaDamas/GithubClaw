# Playwright Testing

## Setup
- Ensure the application under test is running locally.
- Use `npx playwright test` for headless execution.
- Capture screenshots at key interaction points for VLM analysis.

## Patterns
- Navigate to the page, wait for network idle.
- Interact with elements using accessible selectors (role, label, text).
- Assert visible outcomes, not implementation details.
- On failure, capture a screenshot and describe what you see.
