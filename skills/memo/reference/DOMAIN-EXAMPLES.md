# Domain Knowledge Examples

Examples of domain knowledge files written to `~/.claude/knowledges/{project}/domain/`.

---

## Entity Example

**File:** `domain/user.md`

```markdown
---
name: user
description: Platform user authentication, roles, and lifecycle
keywords: user, auth, role, email, status, account, verification
---

## Semantic Meaning

Users access the platform via email identity. Each user has an authorization level (role)
and an account state (status). Email is the canonical unique identifier.

## Key Concepts

- **email**: Unique identifier, used for login
- **role**: Authorization level (`admin` | `user` | `guest`)
- **status**: Account state (`active` | `suspended` | `deleted`)
- **verified**: Email confirmation flag — required for full feature access

## Business Rules

- Email must be globally unique
- Hard delete is prohibited (soft delete via status)
- New users default to `role=user`, `status=active`, `verified=false`
- Unverified users cannot access restricted features

## Related

- Role values: `~/.claude/knowledges/{project}/domain/user-role.md`
- User-team relationship: `~/.claude/knowledges/{project}/domain/user+team.md`
```

---

## Workflow Example

**File:** `domain/order-workflow.md`

```markdown
---
name: order-workflow
description: Order lifecycle from cart to delivery including cancellation rules
keywords: order, workflow, state, pending, confirmed, shipped, delivered, cancel, refund
---

## States

`pending` → `confirmed` → `shipped` → `delivered`
`pending | confirmed` → `cancelled`

## Transitions

| Trigger | From | To | Side Effects |
|---------|------|----|--------------|
| Payment success | pending | confirmed | Deduct inventory, send confirmation email |
| Fulfillment ships | confirmed | shipped | Add tracking number, send shipping email |
| Carrier confirms | shipped | delivered | Enable review/feedback |
| User/admin cancels | pending/confirmed | cancelled | Return inventory, process refund |

## Business Rules

- Inventory locked 15 min during pending; order deleted on timeout
- Cancellation only allowed within 24 hours of confirmation
- Refunds take 3–5 business days
```

---

## Relationship Example

**File:** `domain/user+team.md`

```markdown
---
name: user+team
description: Many-to-one relationship between users and teams — data access scope
keywords: user, team, relationship, multi-tenancy, membership, role, scope
---

## Relationship Type

Many-to-One: multiple users belong to one team.

## Semantic Meaning

A user's team determines their data access scope (multi-tenancy boundary).
Team membership is mandatory and admin-controlled.

## Business Rules

- Users cannot change their own team
- Deleting a team requires reassigning or deleting all members
- Team admins manage users within their team only — no cross-team access
```
