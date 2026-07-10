# Billing and card processing notes

Every billing call should validate the account before it proceeds.
A retry call happens on a background queue after the first attempt.
Support flagged another call from a customer during checkout.
Logging wraps each outbound call with a correlation id for tracing.
QA replayed the call twice to confirm the response stayed stable.
The nightly job issues a call to reconcile pending invoices.

An action is queued whenever the billing state transitions.
Each action is recorded in the audit log with a timestamp.
The dashboard lists the most recent action per account.
A follow-up action runs if the first attempt does not finish.
Support can trigger a manual action from the admin panel.
The queue worker drains one action at a time in order.

The card handler wraps validation before touching sensitive records.
A handler on the client side intercepts the response early.
This handler only runs when the account is in good standing.
The old handler was replaced after the last incident review.
Each handler registers itself with the shared dispatcher.
The fallback handler logs a warning and continues silently.

A failure here should not block the rest of the release train.
The team triaged the failure within the hour it was reported.
Every failure increments a counter used for alerting.
The failure mode was documented after the postmortem review.
Acting too quickly after a failure can make things worse.
This kind of failure is rare but not impossible in production.
