# Offload: Product Positioning Document

> **Status:** Draft v0.1 — for internal/leadership review
> **Last updated:** 2025-02-23
> **Authors:** [Your name here]

---

## Executive Summary (for Leadership)

### What is Offload?

A test runner that farms tests to cloud compute, so AI coding agents can iterate faster without blocking your laptop.

### Why does this matter now?

AI coding assistants (Sculptor, Claude Code, Cursor, etc.) are becoming central to how developers work. But these agents spend most of their time *waiting*—blocked on test suites running locally. Offload removes that bottleneck.

This isn't a side project. It's **strategic infrastructure for AI-assisted development**—and we're already using it on Sculptor.

### Why us?

We built this to solve our own problem. The Sculptor team needed faster test feedback for agent workflows. We're practitioners, not theorists.

### The plan

| Phase | What | Timeline |
|-------|------|----------|
| **Soft launch** | Release to Discord community. Collect feedback. Find sharp edges. | This week |
| **Iterate** | Fix friction points. Refine messaging based on what resonates. | 2-4 weeks |
| **Go/no-go decision** | Evaluate results. Decide whether to go public. | End of iteration period |
| **Public launch** | HN, LinkedIn, broader channels—only after validation. | TBD based on results |

### Success metrics (soft launch)

| Metric | Target |
|--------|--------|
| Discord users who try it | 10+ |
| Users who give substantive feedback | 3+ |
| Quotable testimonials collected | 1-2 |
| Top friction points identified | 3 |

### The ask

Support for 2-4 weeks to run this experiment with our existing Discord community. Clear go/no-go decision point at the end.

### Worst-case outcome

Even if Offload doesn't become a standalone product:
- We've built infrastructure that makes Sculptor better
- We've learned about our users' agent workflows
- We've built goodwill with our community by shipping something useful
- The code remains usable internally

**This is a cheap option with capped downside and meaningful upside.**

---

## 1. Category

**What shelf does this go on?**

Developer tooling / Test infrastructure / AI agent enablement

Offload is a **parallel test runner with pluggable cloud execution**. But the strategic framing is: **infrastructure that unblocks AI coding agents**.

---

## 2. Target User

**Who specifically feels this pain?**

**Primary:** Senior developers at companies with <50 employees who are:
- Using AI coding assistants (Sculptor, Claude Code, Cursor, Copilot, etc.)
- Working on codebases with test suites that take >5 minutes to run
- Frustrated that their agent workflows are bottlenecked by test execution

**Secondary:** Hobbyist developers / indie hackers who:
- Are early adopters of AI-assisted development
- Don't have access to enterprise CI infrastructure
- Want to move fast without waiting

**Not a fit (for now):**
- FAANG employees (they have internal distributed test infrastructure)
- Teams with <2 minute test suites (pain isn't acute enough)
- Organizations with heavy compliance requirements around cloud execution

---

## 3. The Problem

**What sucks about the status quo?**

AI coding assistants are getting good at writing code. But they're terrible at *waiting*.

The iteration loop for an AI agent looks like:
1. Agent writes code
2. Agent runs tests
3. **Agent waits 10-20 minutes** ← bottleneck
4. Agent sees results, iterates

When your test suite takes 15 minutes on 4 cores, your AI assistant spends most of its time blocked. You can throw more cores at it locally, but then:
- Your laptop becomes unusable (pegged at 100% CPU)
- You can't run multiple agents in parallel
- You're still limited by your local machine's ceiling

**The pain in the user's words:**
- "I kicked off the agent and went to get coffee. Twice."
- "My laptop sounds like it's about to take off"
- "I can only run one agent at a time because tests eat all my resources"

---

## 4. Alternatives (What they do today)

| Alternative | Why it falls short |
|-------------|-------------------|
| **Run tests locally on 4 cores** | Slow. 15+ minute feedback loops. |
| **Run tests locally on all cores** | Fast-ish, but laptop is unusable. Can't multitask. |
| **Wait for CI** | Even slower. Not designed for rapid iteration. |
| **Skip tests / YOLO** | Technical debt. Bugs ship. |
| **Enterprise distributed test infra** | Doesn't exist at small companies. |

---

## 5. Unique Capabilities

**What can Offload do that matters?**

| Capability | Why it matters |
|------------|---------------|
| **Parallel execution on cloud sandboxes** | Tests run on Modal's infrastructure, not your laptop. Your machine stays usable. |
| **Pluggable provider model** | Run locally during development, offload to cloud when you need speed. Same config. |
| **Framework-agnostic discovery** | Works with pytest, cargo test, or any custom command. |
| **Automatic retry for flaky tests** | AI agents don't get stuck on intermittent failures. |
| **Simple TOML config** | No complex setup. `offload init && offload run`. |

**The headline capability:** Run your tests on cloud compute with one command, so your AI agents can iterate without blocking your machine.

---

## 6. Proof Points

**Why should they believe us?**

- **Internal usage:** Offload is used internally at Imbue to accelerate agent-driven development on Sculptor
- **Measured improvement:** Cuts test execution time in half compared to 4-core local execution
- **No laptop tax:** Unlike maxing out local cores, offload doesn't peg your machine at 100%
- **Built by practitioners:** Created by the team building Sculptor—we eat our own dogfood

> TODO: Add specific before/after numbers once we have consistent internal usage data
> TODO: Collect testimonials from early Discord users after soft launch

---

## 7. One-Liner

**The thing they'll remember and repeat**

*Working options (pick one, test with users):*

1. "Cloud compute for your test suite. Your AI agent iterates faster, your laptop stays cool."

2. "Offload your tests to the cloud so your AI assistant doesn't have to wait."

3. "Stop waiting for tests. Offload them."

4. "Your AI coding assistant is only as fast as your test suite. Make it faster."

---

## 8. Call to Action

**What do we want them to do?**

**For soft launch (Discord):**
> Download Offload, try it on your project, and tell us what breaks.
>
> ```bash
> cargo install --git https://github.com/imbue-ai/offload
> offload init --provider modal --framework pytest
> offload run
> ```
>
> Drop feedback in #offload-beta or DM [your name].

**Success metrics for soft launch:**
- [ ] 10+ Discord members try it
- [ ] 3+ provide substantive feedback
- [ ] Identify top 3 friction points in setup/usage
- [ ] Collect 1-2 quotable testimonials

---

## 9. Competitive Landscape

| Competitor | What they do | How Offload differs |
|------------|--------------|---------------------|
| **pytest-xdist** | Local parallel test execution | Cloud execution, not just local parallelism |
| **CircleCI / GitHub Actions** | CI/CD with parallelism | Offload is for rapid local iteration, not CI |
| **Buildkite** | Scalable CI | Same—Offload is dev-time, not CI-time |
| **Nx / Turborepo** | Monorepo build caching | Offload is test execution, not build orchestration |

**Our lane:** Developer-time test execution with cloud compute. Not CI. Not build systems. The gap between "I wrote code" and "I know if it works."

---

## 10. Messaging by Channel

**Discord announcement (soft launch):**
> Hey folks—we've been building something internally and want to get your feedback.
>
> **Offload** is a test runner that farms your tests out to cloud compute (Modal) so you're not waiting on your laptop. We built it because our AI agents kept getting bottlenecked by slow test suites.
>
> If you've got a pytest or cargo test project with a test suite that takes more than a few minutes, we'd love for you to try it:
> [install instructions]
>
> This is early. Things will break. That's why we're asking you first.

**HN/LinkedIn (future public launch):**
> [TODO: Draft after soft launch learnings]

---

## 11. Open Questions

- What's the right pricing model? (Free forever? Usage-based? Freemium?)
- Should we support other cloud providers (Fly.io, AWS Lambda, etc.) before public launch?
- Is "AI agent enablement" the right primary frame, or is "faster tests" more universally understood?
- Do we need a hosted version, or is CLI-only sufficient for our audience?

---

## Appendix: Positioning Canvas Summary

| Element | Answer |
|---------|--------|
| **Category** | Test runner with cloud execution |
| **Target** | Senior devs at small companies using AI coding assistants |
| **Problem** | AI agents bottlenecked by slow local test execution |
| **Alternatives** | Local parallel, wait for CI, skip tests |
| **Differentiation** | Cloud sandboxes, pluggable providers, simple config |
| **Proof** | Internal usage, 2x speedup, built by Sculptor team |
| **One-liner** | TBD after user testing |
