# Coding Knowledge Examples

Examples of coding files written to `~/.claude/knowledges/{project}/coding/`.

---

## Pattern Example

**File:** `coding/error-handling.md`

```markdown
---
name: error-handling
description: Standard error class hierarchy and API response format
keywords: error, exception, AppError, ValidationError, NotFoundError, status-code, middleware
---

## Error Classes

Base class — all custom errors extend this:

```typescript
export class AppError extends Error {
  constructor(
    public code: string,
    public message: string,
    public statusCode: number = 500,
    public details?: object
  ) {
    super(message);
    this.name = this.constructor.name;
  }
}

export class ValidationError extends AppError {
  constructor(message: string, details?: object) {
    super('VALIDATION_ERROR', message, 400, details);
  }
}

export class NotFoundError extends AppError {
  constructor(resource: string, id?: string) {
    super('NOT_FOUND', id ? `${resource} ${id} not found` : `${resource} not found`, 404);
  }
}
```

## API Error Response Shape

```typescript
{ error: { code: string; message: string; details?: object; timestamp: string } }
```

## Anti-Patterns

- Don't expose `err.stack` in production responses
- Don't use inconsistent formats (`{ msg }` vs `{ error }`)
```

---

## Architecture Example

**File:** `coding/service-layer.md`

```markdown
---
name: service-layer
description: Service class conventions — single responsibility, DI, custom errors
keywords: service, business-logic, dependency-injection, repository, transaction, pattern
---

## Structure

```
src/services/
├── user.service.ts     # one entity = one service
├── order.service.ts
└── payment.service.ts
```

## Pattern

```typescript
export class UserService {
  constructor(private userRepo: UserRepository, private emailSvc: EmailService) {}

  async createUser(data: CreateUserDTO): Promise<User> {
    if (await this.userRepo.existsByEmail(data.email))
      throw new ConflictError('Email already exists');
    const user = await this.userRepo.create(data);
    await this.emailSvc.sendWelcome(user.email);
    return user;
  }
}
```

## Conventions

- Name: `{Entity}Service` (singular)
- Methods: verb-first — `createUser`, `findById`, `updateEmail`
- Errors: throw custom AppError subclasses
- Transactions: managed in service, not controller
- Simple CRUD: call repository directly from controller (skip service)
```

---

## Implementation Doc Example

See @save/reference/IMPL-EXAMPLE.md for a full `coding/impl-*.md` example.

**When to create**: completed feature + non-obvious choices + alternatives rejected.
**File naming**: `coding/impl-{feature-name}.md`

---

## Conventions Example

**File:** `coding/naming.md`

```markdown
---
name: naming
description: File, class, function, variable, and API naming conventions
keywords: naming, convention, camelCase, PascalCase, snake_case, kebab-case, REST
---

## Files

| Type | Convention | Example |
|------|-----------|---------|
| Service | `{entity}.service.ts` | `user.service.ts` |
| Component | PascalCase | `UserProfile.tsx` |
| Utility | kebab-case | `date-utils.ts` |
| Test | `{source}.test.ts` | `user.service.test.ts` |

## TypeScript

```typescript
class UserService {}          // PascalCase classes
function createUser() {}      // camelCase, verb-first
function isAdmin() {}         // Boolean: is/has/can prefix
const MAX_RETRIES = 3;        // Constants: UPPER_SNAKE_CASE
```

## REST Endpoints

Pattern: `/api/{version}/{resource}/{id?}/{action?}`

- Plural resources: `/users` not `/user`
- Kebab multi-word: `/payment-methods`
- Actions are verbs: `/users/:id/activate`
```
