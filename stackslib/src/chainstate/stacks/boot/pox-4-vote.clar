;;
;; @contract voting for the aggregate public key
;;

;; maps dkg round and signer to proposed aggregate public key
(define-map votes {reward-cycle: uint, round: uint, signer: principal} {aggregate-public-key: (buff 33), reward-slots: uint})
;; maps dkg round and aggregate public key to weights of signers supporting this key so far
(define-map tally {reward-cycle: uint, round: uint, aggregate-public-key: (buff 33)} uint)

(define-constant err-not-allowed (err u10000))
(define-constant err-incorrect-reward-cycle (err u10001))
(define-constant err-incorrect-round (err u10002))
(define-constant err-invalid-aggregate-public-key (err u10003))
(define-constant err-duplicate-vote (err u10004))
(define-constant err-invalid-burn-block-height (err u10005))

(define-data-var last-round uint u0)
(define-data-var is-state-1-active bool true)
(define-data-var state-1 {reward-cycle: uint, round: uint, aggregate-public-key: (optional (buff 33)),
    total-votes: uint}  {reward-cycle: u0, round: u0, aggregate-public-key: none, total-votes: u0})
(define-data-var state-2 {reward-cycle: uint, round: uint, aggregate-public-key: (optional (buff 33)),
    total-votes: uint}  {reward-cycle: u0, round: u0, aggregate-public-key: none, total-votes: u0})

;; get voting info by burn block height
(define-read-only (get-info (height uint))
    (ok (at-block (unwrap! (get-block-info? id-header-hash height) err-invalid-burn-block-height) (get-current-info))))

;; get current voting info
(define-read-only (get-current-info)
    (if (var-get is-state-1-active) (var-get state-1) (var-get state-2)))

(define-read-only (get-signer-public-key (signer principal) (reward-cycle uint))
    ;; TODO replace with contract-call to pox-4::get-signer-public-key
    ;; defined in PR https://github.com/stacks-network/stacks-core/pull/4092
    ;; (contract-call? .pox-4 get-signer-public-key reward-cycle signer)
    (some 0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20))

(define-read-only (get-signer-slots (signer-public-key (buff 33)) (reward-cycle uint))
    u1000000000000)

(define-read-only (current-reward-cycle)
    u0)

(define-public (vote-for-aggregate-public-key (key (buff 33)) (reward-cycle uint) (round uint) (tapleaves (list 4001 (buff 33))))
    (let ((signer-public-key (unwrap! (get-signer-public-key tx-sender reward-cycle) err-not-allowed))
            ;; one slot, one vote
            (num-slots (get-signer-slots signer-public-key reward-cycle))
            (tally-key {reward-cycle: reward-cycle, round: round, aggregate-public-key: key})
            (new-total (+ num-slots (default-to u0 (map-get? tally tally-key))))
            (current-round (var-get last-round)))
        (asserts! (is-eq reward-cycle (current-reward-cycle)) err-incorrect-reward-cycle)
        (asserts! (is-eq round current-round) err-incorrect-round)
        (asserts! (is-eq (len key) u33) err-invalid-aggregate-public-key)
        (asserts! (map-set votes {reward-cycle: reward-cycle, round: round, signer: tx-sender} {aggregate-public-key: key, reward-slots: num-slots}) err-duplicate-vote)
        (map-set tally tally-key new-total)
        (ok true)))