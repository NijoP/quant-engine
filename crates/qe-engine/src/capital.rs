//! Single-target capital plan and checkpoint workflow. Money is `Decimal`.
//!
//! Stage 0 (paper) until 20:00 IST 23 Nov 2026 -> one trading stage from the actual
//! equity to the single target of INR 50,000 by 22:00 IST 24 Nov 2026 -> final
//! checkpoint (stop entries, flatten, cancel entry orders, verify flat, query balance,
//! reconcile, final reports) -> complete. The former 22:30 IST Stage 1 target
//! (INR 3,000) and its INR 2,000 withdrawal checkpoint were removed by the owner on
//! 23 Nov 2026; the `Stage1Growth` stage now carries the single target and ends in the
//! final checkpoint. `withdrawal_inr` defaults to 0 and no withdrawal is prepared.
//!
//! The controller never executes a transfer or withdrawal: it only records one the
//! user confirmed, together with the balance the user (or a balance query) verified.
//! The risk mode is a function of the stage alone; a missed or distant target never
//! changes it.

use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// 2026-11-23 20:00 IST.
pub const STAGE0_END_MS: u64 = 1_795_444_200_000;
/// 2026-11-24 22:00 IST: the campaign deadline and final checkpoint (was 22:00 IST
/// before the 24 Nov campaign spec; see `campaign::CAMPAIGN_DEADLINE_MS`).
pub const STAGE2_DEADLINE_MS: u64 = 1_795_537_800_000;
/// The trading stage ends at the final deadline (no 22:30 IST checkpoint any more).
pub const STAGE1_CHECKPOINT_MS: u64 = STAGE2_DEADLINE_MS;
/// The single capital target (INR).
pub const TARGET_INR: Decimal = dec!(50000);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapitalPlan {
    pub stage0_end_ms: u64,
    pub stage1_start_inr: Decimal,
    pub stage1_target_inr: Decimal,
    pub stage1_checkpoint_ms: u64,
    pub withdrawal_inr: Decimal,
    pub stage2_expected_start_inr: Decimal,
    pub stage2_target_inr: Decimal,
    pub stage2_deadline_ms: u64,
}

impl Default for CapitalPlan {
    fn default() -> Self {
        Self {
            stage0_end_ms: STAGE0_END_MS,
            stage1_start_inr: dec!(100),
            stage1_target_inr: TARGET_INR,
            stage1_checkpoint_ms: STAGE1_CHECKPOINT_MS,
            withdrawal_inr: Decimal::ZERO,
            stage2_expected_start_inr: Decimal::ZERO,
            stage2_target_inr: dec!(50000),
            stage2_deadline_ms: STAGE2_DEADLINE_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Stage0Paper,
    Stage1Growth,
    Checkpoint1,
    Stage2Protective,
    FinalCheckpoint,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskMode {
    Growth,
    Protective,
}

impl Stage {
    /// Risk mode by stage only. Checkpoints and completion allow no entries.
    pub fn risk_mode(self) -> Option<RiskMode> {
        match self {
            Stage::Stage0Paper | Stage::Stage1Growth => Some(RiskMode::Growth),
            Stage::Stage2Protective => Some(RiskMode::Protective),
            _ => None,
        }
    }
    pub fn entries_allowed(self) -> bool {
        self.risk_mode().is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    StopEntries,
    Flatten,
    CancelEntryOrders,
    VerifyFlat,
    QueryBalance,
    Reconcile,
    PrepareWithdrawal,
    AwaitUserWithdrawal,
    FinalReports,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointKind {
    Stage1,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BalanceOrigin {
    /// The paper ledger: never a CoinDCX balance.
    PaperLedger,
    /// An exchange balance query (s3-live hook), only when live is armed by the owner.
    Exchange,
    /// A balance the user typed in after checking the exchange themselves.
    UserVerified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BalanceSnapshot {
    pub ts_ms: u64,
    pub balance_inr: Decimal,
    pub origin: BalanceOrigin,
    pub note: String,
}

/// Balance query hook. The paper desk uses [`PaperLedgerBalance`]; a live
/// implementation belongs to the owner-armed live path and is not called here.
pub trait BalanceSource {
    fn query_balance(&mut self, now_ms: u64) -> Result<BalanceSnapshot, String>;
}

pub struct PaperLedgerBalance(pub Decimal);
impl BalanceSource for PaperLedgerBalance {
    fn query_balance(&mut self, now_ms: u64) -> Result<BalanceSnapshot, String> {
        Ok(BalanceSnapshot {
            ts_ms: now_ms,
            balance_inr: self.0,
            origin: BalanceOrigin::PaperLedger,
            note: "PAPER ledger balance, not a CoinDCX balance".into(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reconciliation {
    pub ledger_equity_inr: Decimal,
    pub queried_balance_inr: Decimal,
    pub difference_inr: Decimal,
    pub stage_realised_pnl_inr: Decimal,
    pub stage_fees_inr: Decimal,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WithdrawalPrep {
    /// Amount prepared for the user to withdraw manually (0 when the balance is short).
    pub amount_inr: Decimal,
    pub expected_retained_inr: Decimal,
    pub target_met: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub kind: CheckpointKind,
    pub started_ms: u64,
    pub steps: Vec<(Step, StepStatus, String)>,
    pub balance: Option<BalanceSnapshot>,
    pub reconciliation: Option<Reconciliation>,
    pub withdrawal: Option<WithdrawalPrep>,
}

impl Checkpoint {
    fn new(kind: CheckpointKind, now_ms: u64) -> Self {
        let mut steps = vec![
            Step::StopEntries,
            Step::Flatten,
            Step::CancelEntryOrders,
            Step::VerifyFlat,
            Step::QueryBalance,
            Step::Reconcile,
        ];
        match kind {
            CheckpointKind::Stage1 => {
                steps.extend([Step::PrepareWithdrawal, Step::AwaitUserWithdrawal])
            }
            CheckpointKind::Final => steps.push(Step::FinalReports),
        }
        Self {
            kind,
            started_ms: now_ms,
            steps: steps
                .into_iter()
                .map(|s| (s, StepStatus::Pending, String::new()))
                .collect(),
            balance: None,
            reconciliation: None,
            withdrawal: None,
        }
    }
    pub fn status(&self, step: Step) -> Option<StepStatus> {
        self.steps.iter().find(|s| s.0 == step).map(|s| s.1)
    }
    fn set(&mut self, step: Step, st: StepStatus, note: &str) {
        if let Some(s) = self.steps.iter_mut().find(|s| s.0 == step) {
            s.1 = st;
            s.2 = note.to_string();
        }
    }
    fn done(&self, step: Step) -> bool {
        self.status(step) == Some(StepStatus::Done)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WithdrawalRecord {
    pub ts_ms: u64,
    pub amount_inr: Decimal,
    pub retained_balance_inr: Decimal,
    pub user_confirmed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageEvent {
    pub ts_ms: u64,
    pub from: Stage,
    pub to: Stage,
    pub equity_inr: Decimal,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapitalController {
    pub plan: CapitalPlan,
    pub stage: Stage,
    /// True while equity is a paper ledger. The desk never sets this false.
    pub paper: bool,
    pub equity: Decimal,
    pub stage_start_equity: Decimal,
    pub stage_started_ms: u64,
    pub stage_realised_pnl: Decimal,
    pub stage_fees: Decimal,
    pub stage_trades: u32,
    pub stage_wins: u32,
    pub high_water_mark: Decimal,
    pub withdrawals: Vec<WithdrawalRecord>,
    pub checkpoint: Option<Checkpoint>,
    pub history: Vec<StageEvent>,
}

impl CapitalController {
    pub fn new(plan: CapitalPlan, start_equity: Decimal, paper: bool, now_ms: u64) -> Self {
        let mut c = Self {
            plan,
            stage: Stage::Stage0Paper,
            paper,
            equity: start_equity,
            stage_start_equity: start_equity,
            stage_started_ms: now_ms,
            stage_realised_pnl: Decimal::ZERO,
            stage_fees: Decimal::ZERO,
            stage_trades: 0,
            stage_wins: 0,
            high_water_mark: start_equity,
            withdrawals: vec![],
            checkpoint: None,
            history: vec![],
        };
        if now_ms >= c.plan.stage0_end_ms && now_ms < c.plan.stage1_checkpoint_ms {
            c.transition(Stage::Stage1Growth, now_ms, "started after 20:00 IST");
        }
        c
    }

    pub fn equity_f64(&self) -> f64 {
        self.equity.to_f64().unwrap_or(0.0)
    }

    pub fn risk_mode(&self) -> Option<RiskMode> {
        self.stage.risk_mode()
    }

    pub fn entries_allowed(&self) -> bool {
        self.stage.entries_allowed()
    }

    /// Current stage target and deadline.
    pub fn target(&self) -> (Decimal, u64) {
        match self.stage {
            Stage::Stage0Paper | Stage::Stage1Growth | Stage::Checkpoint1 => {
                (self.plan.stage1_target_inr, self.plan.stage1_checkpoint_ms)
            }
            _ => (self.plan.stage2_target_inr, self.plan.stage2_deadline_ms),
        }
    }

    fn transition(&mut self, to: Stage, now_ms: u64, note: &str) {
        self.history.push(StageEvent {
            ts_ms: now_ms,
            from: self.stage,
            to,
            equity_inr: self.equity,
            note: note.into(),
        });
        self.stage = to;
        if to.entries_allowed() {
            self.stage_start_equity = self.equity;
            self.stage_started_ms = now_ms;
            self.stage_realised_pnl = Decimal::ZERO;
            self.stage_fees = Decimal::ZERO;
            self.stage_trades = 0;
            self.stage_wins = 0;
            self.high_water_mark = self.equity;
        }
    }

    /// Book a closed trade's net P&L (after fees) and its fees.
    pub fn on_realised(&mut self, pnl_net: Decimal, fees: Decimal) {
        self.equity += pnl_net;
        if self.equity < Decimal::ZERO {
            self.equity = Decimal::ZERO;
        }
        self.stage_realised_pnl += pnl_net;
        self.stage_fees += fees;
        self.stage_trades += 1;
        if pnl_net > Decimal::ZERO {
            self.stage_wins += 1;
        }
        self.high_water_mark = self.high_water_mark.max(self.equity);
    }

    /// Advance the clock. Returns the transition when one happened; a returned
    /// checkpoint means the desk must stop entries and flatten now.
    pub fn on_clock(&mut self, now_ms: u64) -> Option<StageEvent> {
        let before = self.history.len();
        match self.stage {
            Stage::Stage0Paper if now_ms >= self.plan.stage1_checkpoint_ms => {
                self.begin_checkpoint(CheckpointKind::Final, now_ms, "22:00 IST final checkpoint")
            }
            Stage::Stage0Paper if now_ms >= self.plan.stage0_end_ms => self.transition(
                Stage::Stage1Growth,
                now_ms,
                if self.paper {
                    "20:00 IST: trading toward INR 50,000 by 22:00 IST 24 Nov (PAPER: live not armed)"
                } else {
                    "20:00 IST: trading toward INR 50,000 by 22:00 IST 24 Nov"
                },
            ),
            Stage::Stage1Growth if now_ms >= self.plan.stage1_checkpoint_ms => {
                self.begin_checkpoint(CheckpointKind::Final, now_ms, "22:00 IST final checkpoint")
            }
            Stage::Stage1Growth if self.equity >= self.plan.stage1_target_inr => self
                .begin_checkpoint(
                    CheckpointKind::Final,
                    now_ms,
                    "INR 50,000 target reached on actual equity: final checkpoint",
                ),
            Stage::Stage2Protective if now_ms >= self.plan.stage2_deadline_ms => {
                self.begin_checkpoint(CheckpointKind::Final, now_ms, "22:00 IST final checkpoint")
            }
            Stage::Stage2Protective if self.equity >= self.plan.stage2_target_inr => self
                .begin_checkpoint(
                    CheckpointKind::Final,
                    now_ms,
                    "Stage 2 target reached: final checkpoint",
                ),
            _ => {}
        }
        (self.history.len() > before).then(|| self.history.last().cloned().unwrap())
    }

    fn begin_checkpoint(&mut self, kind: CheckpointKind, now_ms: u64, note: &str) {
        let to = match kind {
            CheckpointKind::Stage1 => Stage::Checkpoint1,
            CheckpointKind::Final => Stage::FinalCheckpoint,
        };
        self.transition(to, now_ms, note);
        self.checkpoint = Some(Checkpoint::new(kind, now_ms));
    }

    /// The desk reports a mechanical step (stop entries, flatten, cancel, verify flat).
    pub fn report_step(&mut self, step: Step, ok: bool, note: &str) -> Result<(), String> {
        let cp = self
            .checkpoint
            .as_mut()
            .ok_or("no checkpoint in progress")?;
        if matches!(
            step,
            Step::QueryBalance | Step::Reconcile | Step::PrepareWithdrawal
        ) {
            return Err("use record_balance for balance, reconcile and withdrawal steps".into());
        }
        cp.set(
            step,
            if ok {
                StepStatus::Done
            } else {
                StepStatus::Failed
            },
            note,
        );
        Ok(())
    }

    /// Query the balance (only after the account is verified flat), reconcile it
    /// against the ledger and, at checkpoint 1, prepare the withdrawal for the user.
    pub fn record_balance(
        &mut self,
        src: &mut dyn BalanceSource,
        now_ms: u64,
    ) -> Result<(), String> {
        let (equity, pnl, fees) = (self.equity, self.stage_realised_pnl, self.stage_fees);
        let (target, w) = (self.plan.stage1_target_inr, self.plan.withdrawal_inr);
        let cp = self
            .checkpoint
            .as_mut()
            .ok_or("no checkpoint in progress")?;
        if !cp.done(Step::VerifyFlat) {
            return Err("account not verified flat; balance not queried".into());
        }
        let snap = match src.query_balance(now_ms) {
            Ok(s) => s,
            Err(e) => {
                cp.set(Step::QueryBalance, StepStatus::Failed, &e);
                return Err(e);
            }
        };
        cp.set(Step::QueryBalance, StepStatus::Done, &snap.note);
        let diff = snap.balance_inr - equity;
        let ok = diff.abs() <= dec!(0.01);
        cp.reconciliation = Some(Reconciliation {
            ledger_equity_inr: equity,
            queried_balance_inr: snap.balance_inr,
            difference_inr: diff,
            stage_realised_pnl_inr: pnl,
            stage_fees_inr: fees,
            ok,
        });
        cp.set(
            Step::Reconcile,
            if ok {
                StepStatus::Done
            } else {
                StepStatus::Failed
            },
            &format!(
                "ledger {equity} vs balance {} (diff {diff})",
                snap.balance_inr
            ),
        );
        if cp.kind == CheckpointKind::Stage1 {
            let bal = snap.balance_inr;
            let amount = if bal >= w { w } else { Decimal::ZERO };
            let met = bal >= target;
            cp.withdrawal = Some(WithdrawalPrep {
                amount_inr: amount,
                expected_retained_inr: bal - amount,
                target_met: met,
                note: if met {
                    format!("Stage 1 target met on the queried balance; INR {amount} prepared for the user to withdraw manually")
                } else if amount > Decimal::ZERO {
                    format!("Stage 1 target missed (balance INR {bal} < INR {target}); an INR {amount} withdrawal is still possible, the user decides; risk limits unchanged")
                } else {
                    format!("Stage 1 target missed (balance INR {bal}); no INR {w} withdrawal possible; risk limits unchanged")
                },
            });
            cp.set(
                Step::PrepareWithdrawal,
                StepStatus::Done,
                "prepared, not executed",
            );
        }
        cp.balance = Some(snap);
        Ok(())
    }

    /// Record the user's manual withdrawal (or none: amount 0) and the retained balance
    /// they verified, then start Stage 2 from that balance. Never executes a transfer.
    pub fn confirm_withdrawal(
        &mut self,
        amount: Decimal,
        user_confirmed: bool,
        verified_retained: Decimal,
        now_ms: u64,
    ) -> Result<Decimal, String> {
        if self.stage != Stage::Checkpoint1 {
            return Err("not at the Stage 1 checkpoint".into());
        }
        if !user_confirmed {
            return Err("withdrawal not confirmed by the user; nothing recorded".into());
        }
        let cp = self
            .checkpoint
            .as_mut()
            .ok_or("no checkpoint in progress")?;
        if !cp.done(Step::VerifyFlat) {
            return Err("account not verified flat".into());
        }
        if cp.balance.is_none() {
            return Err("balance not queried and reconciled yet".into());
        }
        if amount < Decimal::ZERO || verified_retained < Decimal::ZERO {
            return Err("negative amount or balance".into());
        }
        cp.set(
            Step::AwaitUserWithdrawal,
            StepStatus::Done,
            "user confirmed",
        );
        self.withdrawals.push(WithdrawalRecord {
            ts_ms: now_ms,
            amount_inr: amount,
            retained_balance_inr: verified_retained,
            user_confirmed,
        });
        self.equity = verified_retained;
        self.transition(
            Stage::Stage2Protective,
            now_ms,
            &format!("Stage 2 protective from verified retained INR {verified_retained}"),
        );
        Ok(self.equity)
    }

    /// Finish the final checkpoint once reports are written.
    pub fn complete_final(&mut self, now_ms: u64) -> Result<(), String> {
        let cp = self
            .checkpoint
            .as_mut()
            .ok_or("no checkpoint in progress")?;
        if cp.kind != CheckpointKind::Final || !cp.done(Step::VerifyFlat) || cp.balance.is_none() {
            return Err("final checkpoint: verify flat and reconcile first".into());
        }
        cp.set(Step::FinalReports, StepStatus::Done, "reports written");
        self.transition(Stage::Complete, now_ms, "final checkpoint complete");
        Ok(())
    }

    /// Target-implied requirements from the actual (ledger) equity.
    pub fn required(&self, now_ms: u64) -> crate::frequency::Requirement {
        let (target, deadline) = self.target();
        let hours = deadline.saturating_sub(now_ms) as f64 / 3.6e6;
        crate::frequency::Requirement::new(self.equity_f64(), target.to_f64().unwrap_or(0.0), hours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = STAGE0_END_MS - 3_600_000;

    fn ctl() -> CapitalController {
        CapitalController::new(CapitalPlan::default(), dec!(100), true, T0)
    }

    fn flat(c: &mut CapitalController) {
        for s in [
            Step::StopEntries,
            Step::Flatten,
            Step::CancelEntryOrders,
            Step::VerifyFlat,
        ] {
            c.report_step(s, true, "ok").unwrap();
        }
    }

    #[test]
    fn plan_times_are_ist() {
        assert_eq!(STAGE2_DEADLINE_MS - STAGE0_END_MS, 93_600_000); // 26 h
        assert_eq!(
            STAGE2_DEADLINE_MS as i64,
            crate::campaign::CAMPAIGN_DEADLINE_MS
        );
        assert_eq!(STAGE1_CHECKPOINT_MS, STAGE2_DEADLINE_MS); // no 22:30 checkpoint
                                                              // 2026-11-23 14:30:00 UTC == 20:00 IST
        assert_eq!(STAGE0_END_MS / 1000 % 86_400, 14 * 3600 + 30 * 60);
        let p = CapitalPlan::default();
        assert_eq!(p.stage1_target_inr, dec!(50000));
        assert_eq!(p.withdrawal_inr, Decimal::ZERO);
    }

    #[test]
    fn single_target_reached_goes_to_final_checkpoint_without_withdrawal() {
        let mut c = ctl();
        assert_eq!(c.stage, Stage::Stage0Paper);
        assert_eq!(c.risk_mode(), Some(RiskMode::Growth));
        assert!(c.on_clock(STAGE0_END_MS).is_some());
        assert_eq!(c.stage, Stage::Stage1Growth);
        assert_eq!(c.target(), (dec!(50000), STAGE2_DEADLINE_MS));
        // the old INR 3,000 / 22:30 target no longer ends the stage
        c.on_realised(dec!(2900), dec!(1));
        assert!(c.on_clock(STAGE0_END_MS + 9_000_000).is_none());
        assert_eq!(c.stage, Stage::Stage1Growth);
        c.on_realised(dec!(47000), dec!(1));
        assert_eq!(c.equity, dec!(50000));
        c.on_clock(STAGE0_END_MS + 9_000_001);
        assert_eq!(c.stage, Stage::FinalCheckpoint);
        assert!(!c.entries_allowed());
        flat(&mut c);
        c.record_balance(&mut PaperLedgerBalance(dec!(50000)), 3)
            .unwrap();
        let cp = c.checkpoint.clone().unwrap();
        assert_eq!(cp.kind, CheckpointKind::Final);
        assert!(cp.reconciliation.unwrap().ok);
        assert!(cp.withdrawal.is_none());
        c.complete_final(4).unwrap();
        assert_eq!(c.stage, Stage::Complete);
    }

    #[test]
    fn missed_target_never_raises_risk() {
        let mut c = ctl();
        c.on_clock(STAGE0_END_MS);
        c.on_realised(dec!(-3), dec!(0.1));
        assert_eq!(c.risk_mode(), Some(RiskMode::Growth));
        assert!(c.on_clock(STAGE2_DEADLINE_MS - 1).is_none());
        assert_eq!(c.risk_mode(), Some(RiskMode::Growth));
        c.on_clock(STAGE2_DEADLINE_MS);
        assert_eq!(c.stage, Stage::FinalCheckpoint);
        assert_eq!(c.risk_mode(), None);
    }

    #[test]
    fn reconciliation_flags_mismatch() {
        let mut c = ctl();
        c.on_clock(STAGE1_CHECKPOINT_MS);
        flat(&mut c);
        c.record_balance(&mut PaperLedgerBalance(dec!(99.5)), 1)
            .unwrap();
        let cp = c.checkpoint.unwrap();
        let r = cp.reconciliation.clone().unwrap();
        assert!(!r.ok);
        assert_eq!(r.difference_inr, dec!(-0.5));
        assert_eq!(cp.status(Step::Reconcile), Some(StepStatus::Failed));
    }

    #[test]
    fn final_checkpoint_at_deadline() {
        let mut c = ctl();
        c.on_clock(STAGE0_END_MS);
        assert!(c.on_clock(STAGE2_DEADLINE_MS - 1).is_none());
        c.on_clock(STAGE2_DEADLINE_MS);
        assert_eq!(c.stage, Stage::FinalCheckpoint);
        assert!(!c.entries_allowed());
        assert!(c.complete_final(3).is_err());
        flat(&mut c);
        c.record_balance(&mut PaperLedgerBalance(dec!(100)), 4)
            .unwrap();
        c.complete_final(5).unwrap();
        assert_eq!(c.stage, Stage::Complete);
    }
}
