---
name: infrastructure-review
description: Review an infrastructure or configuration change (Terraform, Kubernetes manifests, cloud config) for blast radius, state drift, and silent risk before it applies. Reads plan output for what is destroyed and replaced, not only created. Not for application code diffs -- that is review.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: infrastructure-review
  x-zirv-version: "1"
  x-zirv-name: Infrastructure review
  x-zirv-triggers: terraform plan,infra change,kubernetes manifest,iac review,cloudformation,pulumi
  x-zirv-phases: review
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "1900"
---

A plan's "to add" list is reassuring and incomplete. The changes that cause
outages are usually in the destroy and replace lines, and a plan that looks
green can still be diffing against a world that no longer exists.

## Method

1. Read every destroy and replace action before the creates. A replace is a
   delete plus a create in sequence, and for stateful resources -- volumes,
   databases, DNS records -- that sequence can mean data loss or an
   availability gap the create list never shows.
2. Check whether the plan's baseline still matches reality. State drift -- a
   manual console change, an out-of-band fix, a resource renamed outside the
   tool -- means the plan's actions are unreliable until refreshed.
3. Establish blast radius: which environment, which region, how many
   dependents call the resources being changed, and whether the change is
   reversible without redeploying from scratch.
4. Look specifically for widened permissions (a broadened IAM policy, a new
   public ingress rule, a security group opened wider), secrets moving from a
   vault reference into plaintext, and cost that arrives silently -- a
   resource class change, a new always-on service, cross-region transfer.
5. Say plainly what the plan output cannot tell you: whether a resource is
   actually receiving traffic, whether a dependent service will handle the
   interruption, whether the change was tested anywhere before this. A plan
   proves syntax and intended state, not operational safety.

## Contract

Report the destroy/replace list with what each affects, drift found and
whether it invalidates the plan, blast radius, any permission widening or
plaintext secret exposure, and the cost delta if visible. State plainly which
risks the plan output cannot answer rather than inferring one from resource
names. Application-level diffs in the same change belong to `review`, not
here.
