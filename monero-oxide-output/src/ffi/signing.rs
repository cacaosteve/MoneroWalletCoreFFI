//! Transaction signing runs on a dedicated, large-stack worker.
//!
//! Bulletproofs+ lazily initializes its generator tables while proving. That initialization can
//! exceed the relatively small stack used by Swift's cooperative executor on iOS, terminating the
//! process with EXC_BAD_ACCESS instead of returning an error. A dedicated native thread gives
//! proof generation a predictable stack on every FFI caller/platform.

use monero_wallet::{
    ed25519::Scalar,
    send::SignableTransaction,
    transaction::{NotPruned, Transaction},
};
use rand::rngs::OsRng;
use zeroize::Zeroizing;

const TRANSACTION_SIGNING_STACK_SIZE: usize = 8 * 1024 * 1024;

pub(crate) fn sign_transaction_on_large_stack(
    intent: SignableTransaction,
    spend_key: Zeroizing<Scalar>,
) -> Result<Transaction<NotPruned>, String> {
    std::thread::Builder::new()
        .name("nexawal-transaction-signing".to_string())
        .stack_size(TRANSACTION_SIGNING_STACK_SIZE)
        .spawn(move || {
            let mut rng = OsRng;
            intent
                .sign(&mut rng, &spend_key)
                .map_err(|error| error.to_string())
        })
        .map_err(|error| format!("failed to start transaction signing worker: {error}"))?
        .join()
        .map_err(|_| "transaction signing worker panicked".to_string())?
}
