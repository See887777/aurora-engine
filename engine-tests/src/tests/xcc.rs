use crate::test_utils::erc20::{ERC20Constructor, ERC20};
use crate::test_utils::{self, AuroraRunner};
use crate::tests::erc20_connector::sim_tests;
use crate::tests::state_migration::{deploy_evm, AuroraAccount};
use aurora_engine_precompiles::xcc::{self, costs, cross_contract_call};
use aurora_engine_transactions::legacy::TransactionLegacy;
use aurora_engine_types::parameters::{
    CrossContractCallArgs, PromiseArgs, PromiseCreateArgs, PromiseWithCallbackArgs,
};
use aurora_engine_types::types::{Address, EthGas, NearGas, Wei, Yocto};
use aurora_engine_types::U256;
use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::transaction::Action;
use near_primitives_core::contract::ContractCode;
use near_sdk_sim::UserAccount;
use serde_json::json;
use std::fs;
use std::path::Path;

const WNEAR_AMOUNT: u128 = 10 * near_sdk_sim::STORAGE_AMOUNT;

#[test]
fn test_xcc_eth_gas_cost() {
    let mut runner = test_utils::deploy_evm();
    runner.standalone_runner = None;
    let xcc_wasm_bytes = contract_bytes();
    let _ = runner.call("factory_update", "aurora", xcc_wasm_bytes);
    let mut signer = test_utils::Signer::random();
    let mut baseline_signer = test_utils::Signer::random();
    runner.context.block_index = aurora_engine::engine::ZERO_ADDRESS_FIX_HEIGHT + 1;
    // Need to use engine's deploy!
    let wnear_erc20 = deploy_erc20(&mut runner, &mut signer);
    approve_erc20(
        &wnear_erc20,
        cross_contract_call::ADDRESS,
        &mut runner,
        &mut signer,
    );
    approve_erc20(
        &wnear_erc20,
        test_utils::address_from_secret_key(&baseline_signer.secret_key),
        &mut runner,
        &mut signer,
    );
    let _ = runner.call(
        "factory_set_wnear_address",
        "aurora",
        wnear_erc20.0.address.as_bytes().to_vec(),
    );

    // Baseline transaction is an ERC-20 transferFrom call since such a call is included as part
    // of the precompile execution, but we want to isolate just the precompile logic itself
    // (the EVM subcall is charged separately).
    let (baseline_result, baseline) = runner
        .submit_with_signer_profiled(&mut baseline_signer, |nonce| {
            wnear_erc20.transfer_from(
                test_utils::address_from_secret_key(&signer.secret_key),
                Address::from_array([1u8; 20]),
                U256::from(near_sdk_sim::STORAGE_AMOUNT),
                nonce,
            )
        })
        .unwrap();
    if !baseline_result.status.is_ok() {
        panic!("Unexpected baseline status: {:?}", baseline_result);
    }

    let mut profile_for_promise = |p: PromiseArgs| -> (u64, u64, u64) {
        let data = CrossContractCallArgs::Eager(p).try_to_vec().unwrap();
        let input_length = data.len();
        let (submit_result, profile) = runner
            .submit_with_signer_profiled(&mut signer, |nonce| TransactionLegacy {
                nonce,
                gas_price: U256::zero(),
                gas_limit: u64::MAX.into(),
                to: Some(cross_contract_call::ADDRESS),
                value: Wei::zero(),
                data,
            })
            .unwrap();
        assert!(submit_result.status.is_ok());
        // Subtract off baseline transaction to isolate just precompile things
        (
            u64::try_from(input_length).unwrap(),
            profile.all_gas() - baseline.all_gas(),
            submit_result.gas_used,
        )
    };

    let promise = PromiseCreateArgs {
        target_account_id: "some_account.near".parse().unwrap(),
        method: "some_method".into(),
        args: b"hello_world".to_vec(),
        attached_balance: Yocto::new(56),
        attached_gas: NearGas::new(500),
    };
    // Shorter input
    let (x1, y1, evm1) = profile_for_promise(PromiseArgs::Create(promise.clone()));
    // longer input
    let (x2, y2, evm2) = profile_for_promise(PromiseArgs::Callback(PromiseWithCallbackArgs {
        base: promise.clone(),
        callback: promise,
    }));

    // NEAR costs (inferred from a line through (x1, y1) and (x2, y2))
    let xcc_cost_per_byte = (y2 - y1) / (x2 - x1);
    let xcc_base_cost = NearGas::new(y1 - xcc_cost_per_byte * x1);

    // Convert to EVM cost using conversion ratio
    let xcc_base_cost = EthGas::new(xcc_base_cost.as_u64() / costs::CROSS_CONTRACT_CALL_NEAR_GAS);
    let xcc_cost_per_byte = xcc_cost_per_byte / costs::CROSS_CONTRACT_CALL_NEAR_GAS;

    assert!(
        test_utils::within_x_percent(
            5,
            xcc_base_cost.as_u64(),
            costs::CROSS_CONTRACT_CALL_BASE.as_u64()
        ),
        "Incorrect xcc base cost. Expected: {} Actual: {}",
        xcc_base_cost,
        costs::CROSS_CONTRACT_CALL_BASE
    );

    assert!(
        test_utils::within_x_percent(
            5,
            xcc_cost_per_byte,
            costs::CROSS_CONTRACT_CALL_BYTE.as_u64()
        ),
        "Incorrect xcc per byte cost. Expected: {} Actual: {}",
        xcc_cost_per_byte,
        costs::CROSS_CONTRACT_CALL_BYTE
    );

    // As a sanity check, confirm that the total EVM gas spent aligns with expectations
    let total_gas1 = y1 + baseline.all_gas();
    let total_gas2 = y2 + baseline.all_gas();
    assert!(
        test_utils::within_x_percent(20, evm1, total_gas1 / costs::CROSS_CONTRACT_CALL_NEAR_GAS),
        "Incorrect EVM gas used. Expected: {} Actual: {}",
        evm1,
        total_gas1 / costs::CROSS_CONTRACT_CALL_NEAR_GAS
    );
    assert!(
        test_utils::within_x_percent(20, evm2, total_gas2 / costs::CROSS_CONTRACT_CALL_NEAR_GAS),
        "Incorrect EVM gas used. Expected: {} Actual: {}",
        evm2,
        total_gas2 / costs::CROSS_CONTRACT_CALL_NEAR_GAS
    );
}

#[test]
fn test_xcc_precompile_eager() {
    test_xcc_precompile_common(false)
}

#[test]
fn test_xcc_precompile_scheduled() {
    test_xcc_precompile_common(true)
}

fn test_xcc_precompile_common(is_scheduled: bool) {
    let aurora = deploy_evm();
    let chain_id = AuroraRunner::default().chain_id;
    let xcc_wasm_bytes = contract_bytes();
    aurora
        .user
        .call(
            aurora.contract.account_id(),
            "factory_update",
            &xcc_wasm_bytes,
            near_sdk_sim::DEFAULT_GAS,
            0,
        )
        .assert_success();

    let mut signer = test_utils::Signer::random();
    let signer_address = test_utils::address_from_secret_key(&signer.secret_key);

    // Setup wNEAR contract and bridge it to Aurora
    let wnear_account = deploy_wnear(&aurora);
    let wnear_erc20 = sim_tests::deploy_erc20_from_nep_141(&wnear_account, &aurora);
    sim_tests::transfer_nep_141_to_erc_20(
        &wnear_account,
        &wnear_erc20,
        &aurora.user,
        signer_address,
        WNEAR_AMOUNT,
        &aurora,
    );
    aurora
        .user
        .call(
            aurora.contract.account_id(),
            "factory_set_wnear_address",
            wnear_erc20.0.address.as_bytes(),
            near_sdk_sim::DEFAULT_GAS,
            0,
        )
        .assert_success();
    let approve_tx = wnear_erc20.approve(
        cross_contract_call::ADDRESS,
        WNEAR_AMOUNT.into(),
        signer.use_nonce().into(),
    );
    let signed_transaction =
        test_utils::sign_transaction(approve_tx, Some(chain_id), &signer.secret_key);
    aurora
        .user
        .call(
            aurora.contract.account_id(),
            "submit",
            &rlp::encode(&signed_transaction),
            near_sdk_sim::DEFAULT_GAS,
            0,
        )
        .assert_success();

    let router_account = format!(
        "{}.{}",
        hex::encode(signer_address.as_bytes()),
        aurora.contract.account_id.as_str()
    );

    // 1. Deploy NEP-141 token.
    let ft_owner = aurora.user.create_user(
        "ft_owner.root".parse().unwrap(),
        near_sdk_sim::STORAGE_AMOUNT,
    );
    let nep_141_supply = 500;
    let nep_141_token = sim_tests::deploy_nep_141(
        "test_token.root",
        ft_owner.account_id.as_ref(),
        nep_141_supply,
        &aurora,
    );

    // 2. Register EVM router contract
    let args = serde_json::json!({
        "account_id": router_account,
    })
    .to_string();
    aurora
        .user
        .call(
            nep_141_token.account_id(),
            "storage_deposit",
            args.as_bytes(),
            near_sdk_sim::DEFAULT_GAS,
            near_sdk_sim::STORAGE_AMOUNT,
        )
        .assert_success();

    // 3. Give router some tokens
    let transfer_amount: u128 = 199;
    let args = serde_json::json!({
        "receiver_id": router_account,
        "amount": format!("{}", transfer_amount),
    })
    .to_string();
    ft_owner
        .call(
            nep_141_token.account_id(),
            "ft_transfer",
            args.as_bytes(),
            near_sdk_sim::DEFAULT_GAS,
            1,
        )
        .assert_success();
    assert_eq!(
        sim_tests::nep_141_balance_of(ft_owner.account_id.as_str(), &nep_141_token, &aurora),
        nep_141_supply - transfer_amount
    );

    // 4. Use xcc precompile to send those tokens back
    let args = serde_json::json!({
        "receiver_id": ft_owner.account_id.as_str(),
        "amount": format!("{}", transfer_amount),
    })
    .to_string();
    let promise = PromiseCreateArgs {
        target_account_id: nep_141_token.account_id.as_str().parse().unwrap(),
        method: "ft_transfer".into(),
        args: args.into_bytes(),
        attached_balance: Yocto::new(1),
        attached_gas: NearGas::new(100_000_000_000_000),
    };
    let xcc_args = if is_scheduled {
        CrossContractCallArgs::Delayed(PromiseArgs::Create(promise))
    } else {
        CrossContractCallArgs::Eager(PromiseArgs::Create(promise))
    };
    let transaction = TransactionLegacy {
        nonce: signer.use_nonce().into(),
        gas_price: 0u64.into(),
        gas_limit: u64::MAX.into(),
        to: Some(cross_contract_call::ADDRESS),
        value: Wei::zero(),
        data: xcc_args.try_to_vec().unwrap(),
    };
    let signed_transaction =
        test_utils::sign_transaction(transaction, Some(chain_id), &signer.secret_key);
    let engine_balance_before_xcc = get_engine_near_balance(&aurora);
    let result = aurora.user.call(
        aurora.contract.account_id(),
        "submit",
        &rlp::encode(&signed_transaction),
        near_sdk_sim::DEFAULT_GAS,
        0,
    );
    result.assert_success();
    let submit_result: aurora_engine::parameters::SubmitResult = result.unwrap_borsh();
    if !submit_result.status.is_ok() {
        panic!("Unexpected result {:?}", submit_result);
    }

    print_outcomes(&aurora);
    let engine_balance_after_xcc = get_engine_near_balance(&aurora);
    assert!(
        // engine loses less than 0.01 NEAR
        engine_balance_after_xcc.max(engine_balance_before_xcc)
            - engine_balance_after_xcc.min(engine_balance_before_xcc)
            < 10_000_000_000_000_000_000_000,
        "Engine lost too much NEAR funding xcc: Before={:?} After={:?}",
        engine_balance_before_xcc,
        engine_balance_after_xcc,
    );
    let router_balance = aurora
        .user
        .borrow_runtime()
        .view_account(&router_account)
        .unwrap()
        .amount();
    assert!(
        // router loses less than 0.01 NEAR from its allocated funds
        xcc::state::STORAGE_AMOUNT.as_u128() - router_balance < 10_000_000_000_000_000_000_000,
        "Router lost too much NEAR: Balance={:?}",
        router_balance,
    );
    // Router has no wNEAR balance because it all was unwrapped to actual NEAR
    assert_eq!(
        sim_tests::nep_141_balance_of(&router_account, &wnear_account, &aurora),
        0,
    );

    if is_scheduled {
        // The promise was only scheduled, not executed immediately. So the FT balance has not changed yet.
        assert_eq!(
            sim_tests::nep_141_balance_of(ft_owner.account_id.as_str(), &nep_141_token, &aurora),
            nep_141_supply - transfer_amount
        );

        // Now we execute the scheduled promise
        aurora
            .user
            .call(
                router_account.parse().unwrap(),
                "execute_scheduled",
                b"{\"nonce\": \"0\"}",
                near_sdk_sim::DEFAULT_GAS,
                0,
            )
            .assert_success();
    }

    assert_eq!(
        sim_tests::nep_141_balance_of(ft_owner.account_id.as_str(), &nep_141_token, &aurora),
        nep_141_supply
    );
}

fn get_engine_near_balance(aurora: &AuroraAccount) -> u128 {
    aurora
        .user
        .borrow_runtime()
        .view_account(&aurora.contract.account_id.as_str())
        .unwrap()
        .amount()
}

fn print_outcomes(aurora: &AuroraAccount) {
    let rt = aurora.user.borrow_runtime();
    for id in rt.last_outcomes.iter() {
        println!("{:?}=={:?}\n\n", id, rt.outcome(id).unwrap());
    }
}

#[test]
fn test_xcc_schedule_gas() {
    let mut router = deploy_router();

    let promise = PromiseCreateArgs {
        target_account_id: "some_account.near".parse().unwrap(),
        method: "some_method".into(),
        args: b"hello_world".to_vec(),
        attached_balance: Yocto::new(56),
        attached_gas: NearGas::new(100_000_000_000_000),
    };

    let (maybe_outcome, maybe_error) = router.call(
        "schedule",
        "aurora",
        PromiseArgs::Create(promise.clone()).try_to_vec().unwrap(),
    );
    assert!(maybe_error.is_none());
    let outcome = maybe_outcome.unwrap();
    assert!(
        outcome.burnt_gas < costs::ROUTER_SCHEDULE.as_u64(),
        "{:?} not less than {:?}",
        outcome.burnt_gas,
        costs::ROUTER_SCHEDULE
    );
    assert_eq!(outcome.logs.len(), 1);
    assert_eq!(outcome.logs[0], "Promise scheduled at nonce 0");
}

#[test]
fn test_xcc_exec_gas() {
    let mut router = deploy_router();

    let promise = PromiseCreateArgs {
        target_account_id: "some_account.near".parse().unwrap(),
        method: "some_method".into(),
        args: b"hello_world".to_vec(),
        attached_balance: Yocto::new(56),
        attached_gas: NearGas::new(100_000_000_000_000),
    };

    let (maybe_outcome, maybe_error) = router.call(
        "execute",
        "aurora",
        PromiseArgs::Create(promise.clone()).try_to_vec().unwrap(),
    );
    assert!(maybe_error.is_none());
    let outcome = maybe_outcome.unwrap();

    assert!(
        outcome.burnt_gas < costs::ROUTER_EXEC.as_u64(),
        "{:?} not less than {:?}",
        outcome.burnt_gas,
        costs::ROUTER_EXEC
    );
    assert_eq!(outcome.action_receipts.len(), 1);
    assert_eq!(
        outcome.action_receipts[0].0.as_str(),
        promise.target_account_id.as_ref()
    );
    let receipt = &outcome.action_receipts[0].1;
    assert_eq!(receipt.actions.len(), 1);
    let action = &receipt.actions[0];
    match action {
        Action::FunctionCall(function_call) => {
            assert_eq!(function_call.method_name, promise.method);
            assert_eq!(function_call.args, promise.args);
            assert_eq!(function_call.deposit, promise.attached_balance.as_u128());
            assert_eq!(function_call.gas, promise.attached_gas.as_u64());
        }
        other => panic!("Unexpected action {:?}", other),
    };
}

fn deploy_router() -> AuroraRunner {
    let mut router = AuroraRunner::default();
    router.code = ContractCode::new(contract_bytes(), None);

    router.context.current_account_id = "some_address.aurora".parse().unwrap();
    router.context.predecessor_account_id = "aurora".parse().unwrap();

    let init_args = r#"{"wnear_account": "wrap.near", "must_register": true}"#;
    let (maybe_outcome, maybe_error) =
        router.call("initialize", "aurora", init_args.as_bytes().to_vec());
    assert!(maybe_error.is_none());
    let outcome = maybe_outcome.unwrap();
    assert!(outcome.used_gas < aurora_engine::xcc::INITIALIZE_GAS.as_u64());

    router
}

fn deploy_wnear(aurora: &AuroraAccount) -> UserAccount {
    let contract_bytes = std::fs::read("src/tests/res/w_near.wasm").unwrap();

    let account_id = format!("wrap.{}", aurora.user.account_id.as_str());
    let contract_account = aurora.user.deploy(
        &contract_bytes,
        account_id.parse().unwrap(),
        5 * near_sdk_sim::STORAGE_AMOUNT,
    );

    aurora
        .user
        .call(
            contract_account.account_id(),
            "new",
            &[],
            near_sdk_sim::DEFAULT_GAS,
            0,
        )
        .assert_success();

    // Need to register Aurora contract so that it can receive tokens
    let args = json!({
        "account_id": &aurora.contract.account_id,
    })
    .to_string();
    aurora
        .user
        .call(
            contract_account.account_id(),
            "storage_deposit",
            args.as_bytes(),
            near_sdk_sim::DEFAULT_GAS,
            near_sdk_sim::STORAGE_AMOUNT,
        )
        .assert_success();

    // Need to also register root account
    let args = json!({
        "account_id": &aurora.user.account_id,
    })
    .to_string();
    aurora
        .user
        .call(
            contract_account.account_id(),
            "storage_deposit",
            args.as_bytes(),
            near_sdk_sim::DEFAULT_GAS,
            near_sdk_sim::STORAGE_AMOUNT,
        )
        .assert_success();

    // Mint some wNEAR for the root account to use
    aurora
        .user
        .call(
            contract_account.account_id(),
            "near_deposit",
            &[],
            near_sdk_sim::DEFAULT_GAS,
            WNEAR_AMOUNT,
        )
        .assert_success();

    contract_account
}

fn deploy_erc20(runner: &mut AuroraRunner, signer: &mut test_utils::Signer) -> ERC20 {
    let engine_account = runner.aurora_account_id.clone();
    let args = aurora_engine::parameters::DeployErc20TokenArgs {
        nep141: "wrap.near".parse().unwrap(),
    };
    let (maybe_output, maybe_error) = runner.call(
        "deploy_erc20_token",
        &engine_account,
        args.try_to_vec().unwrap(),
    );
    assert!(maybe_error.is_none());
    let output = maybe_output.unwrap();
    let address = {
        let bytes: Vec<u8> =
            BorshDeserialize::try_from_slice(output.return_data.as_value().as_ref().unwrap())
                .unwrap();
        Address::try_from_slice(&bytes).unwrap()
    };

    let contract = ERC20(ERC20Constructor::load().0.deployed_at(address));
    let dest_address = test_utils::address_from_secret_key(&signer.secret_key);
    let call_args =
        aurora_engine::parameters::CallArgs::V1(aurora_engine::parameters::FunctionCallArgsV1 {
            contract: address,
            input: contract
                .mint(dest_address, WNEAR_AMOUNT.into(), U256::zero())
                .data,
        });
    let (_, maybe_error) = runner.call("call", &engine_account, call_args.try_to_vec().unwrap());
    assert!(maybe_error.is_none());

    contract
}

fn approve_erc20(
    token: &ERC20,
    spender: Address,
    runner: &mut AuroraRunner,
    signer: &mut test_utils::Signer,
) {
    let approve_result = runner
        .submit_with_signer(signer, |nonce| {
            token.approve(spender, WNEAR_AMOUNT.into(), nonce)
        })
        .unwrap();
    assert!(approve_result.status.is_ok());
}

fn contract_bytes() -> Vec<u8> {
    let base_path = Path::new("../etc").join("xcc-router");
    let output_path = base_path.join("target/wasm32-unknown-unknown/release/xcc_router.wasm");
    test_utils::rust::compile(base_path);
    fs::read(output_path).unwrap()
}
