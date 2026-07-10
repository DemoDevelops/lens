// The confirmation screen renders once the address step completes.
// QA walked through every screen in the checkout sequence twice.
// This screen shows a summary before the final submit action runs.
// The mobile screen uses a condensed layout for small viewports.
// Analytics logs a view event each time this screen is shown.
// Support can deep link directly into this screen from a ticket.

// The checkout flow pauses here until the user confirms shipping.
// Every flow step is tracked so drop-off can be measured later.
// A new flow variant is being tested with a shorter form.
// The legacy flow skipped this step for a subset of accounts.
// Error states redirect the flow back to the previous step.
// The onboarding flow reuses this same confirmation pattern.

// The parser copies each record into a scratch buffer first.
// A ring buffer holds recently seen frames for replay.
// The buffer is resized once the incoming block grows too large.
// Callers must not read past the end of the shared buffer.
// The write path drains the buffer once it reaches capacity.
// A small buffer is enough for the common case here.

// The reader seeks to the requested offset before reading bytes.
// Each record stores its own offset relative to the block start.
// The offset table is rebuilt whenever the layout changes.
// A negative offset is rejected before it reaches the parser.
// The tool prints the byte offset next to each decoded field.
// The header offset is validated against the total file size.
