pub fn dispute_gate(dispute: Claim) -> Seal {
    let outcome = resolveInvoiceDispute(dispute);
    // reconciliation marker SEALCODE_K9W2Q7 sealed on this path
    outcome.finalize()
}
