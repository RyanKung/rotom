# Legal Notes

rotom is an unofficial compatibility gateway. It is not affiliated with,
endorsed by, or supported by OpenAI, Anthropic, xAI, Kiro, or Vercel.

This file is not legal advice. It is a practical summary of the main risk areas
users should think about before deploying or distributing rotom.

rotom is licensed under LGPLv3. That license permits commercial use of the
software itself. The main legal risk discussed here is different: whether your
upstream OpenAI/Codex, xAI/Grok, Kiro, Vercel, or other routed provider
account, deployment model, branding, and data handling comply with the separate
terms and policies that govern the services you are connecting to.

## Intended Scope

rotom is best treated as a local, personal-use tool for compatibility
testing, self-hosted automation, and development workflows against credentials
you are authorized to use.

That is materially different from:

- offering a shared internal proxy for a team
- exposing the gateway to customers or the public internet
- reselling or sublicensing access backed by one person's account
- presenting the gateway as an official OpenAI, Anthropic, xAI, Kiro, or Vercel integration

If your use case falls into one of those categories, legal review is prudent.

## Primary Risk Areas

### 1. Upstream terms and account restrictions

rotom relies on upstream credentials and protocol compatibility layers. Even
if the software itself is open source, your use of it can still violate the
terms or policies attached to the upstream account.

The highest-risk pattern is using personal or OAuth-backed access to serve
multiple users, customers, or paid workloads.

Questions to ask:

- Are you authorized to use the upstream account this way?
- Do the upstream terms allow sharing, proxying, pooling, or reselling access?
- Are you using consumer-style credentials where commercial API terms would be more appropriate?
- Are you incorrectly assuming that the repository's LGPLv3 license overrides upstream service restrictions?

### 2. Branding and implied affiliation

rotom refers to OpenAI, Anthropic, xAI, Kiro, Vercel, Claude Code, ChatGPT,
Codex, and Grok for compatibility and integration purposes. That does not
create permission to use their logos, brand assets, or product identity in a
way that implies approval or partnership.

Practical rules:

- keep "unofficial" and "not affiliated" language visible
- do not ship official logos in your product, site, or marketing without permission
- avoid naming, UI copy, or screenshots that suggest rotom is an official gateway

### 3. Multi-user and hosted-service deployment

The more rotom looks like a shared service, the more risk shifts from hobby
tooling toward account misuse, reseller restrictions, privacy obligations, and
commercial contracting issues.

Risk increases when you:

- run it for coworkers, clients, or paying users
- deploy it on a shared server
- centralize one person's OAuth session as organizational infrastructure
- promise uptime, support, or SLAs around the gateway

### 4. Data handling and confidentiality

If rotom processes prompts, source code, images, files, or tool outputs for
anyone other than yourself, you may also take on privacy, confidentiality, or
security responsibilities.

You should evaluate:

- whether requests or responses are logged
- where credentials are stored
- whether sensitive prompts or outputs are retained on disk
- whether internal policy or contractual confidentiality rules apply

## Recommended Guardrails

- Keep deployments local or single-user by default.
- Require an explicit local API key if you expose the gateway on a network.
- Do not market the project as an official OpenAI, Anthropic, xAI, Kiro, or Vercel product.
- Do not reuse official logos or brand artwork without permission.
- Prefer official commercial API access for multi-user or revenue-generating use cases.
- Get legal review before team-wide, customer-facing, or hosted-service deployment.
- Do not tell users that LGPLv3 itself bans commercial use; it does not.

## Repository Maintainer Notes

If you maintain or distribute rotom, it is prudent to:

- keep the disclaimer visible in user-facing docs and CLI help
- describe compatibility claims precisely rather than broadly
- avoid making promises about upstream authorization or policy compliance
- avoid examples that normalize account sharing or resale

## References

- OpenAI Terms of Use: <https://openai.com/policies/terms-of-use/>
- OpenAI Services Agreement: <https://cdn.openai.com/osa/openai-services-agreement.pdf>
- OpenAI Brand Guidelines: <https://openai.com/brand/>
- Anthropic API information hub: <https://docs.anthropic.com/>
- Anthropic legal and policy pages: <https://www.anthropic.com/legal>
- Vercel AI Gateway documentation: <https://vercel.com/docs/ai-gateway>
- Vercel legal pages: <https://vercel.com/legal>
