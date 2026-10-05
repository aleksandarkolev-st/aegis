name: Pull request
description: Propose a change to Aegis
labels: []
body:
  - type: markdown
    attributes:
      value: |
        See [CONTRIBUTING.md](../blob/main/CONTRIBUTING.md) for the project's
        expectations, especially around permissions, evidence, and credential
        storage.

  - type: textarea
    id: summary
    attributes:
      label: Summary
      description: What changed and why.
    validations:
      required: true

  - type: textarea
    id: verification
    attributes:
      label: How this was verified
      description: |
        Which commands you ran and what passed. State plainly what you could
        not verify — passing unit tests do not establish interactive rendering
        or live provider behaviour.
      value: |
        - cargo test --all-targets
        - npm test
    validations:
      required: true

  - type: textarea
    id: risk
    attributes:
      label: Risk and rollback
      description: |
        What could this break, and how would you revert it? Required in full if
        the change touches permissions, credential storage, completion evidence,
        the relay, or the release workflow.
    validations:
      required: false

  - type: checkboxes
    id: checklist
    attributes:
      label: Checklist
      options:
        - label: I did not weaken or skip an existing assertion to get this passing.
        - label: Tests pass on Windows, Linux x64, or Linux arm64 as applicable.
        - label: Any new user-facing error message is covered by a test.
        - label: Documentation updated where behaviour changed.