use cosmwasm_std::{
    attr, entry_point, to_binary, Binary, CanonicalAddr, Deps, DepsMut, Env, MessageInfo, ReplyOn,
    Response, StdError, StdResult, SubMsg, WasmMsg,
};

use crate::error::ContractError;
use crate::querier::query_liquidity_token;
use crate::state::{pair_key, read_pairs, Config, CONFIG, PAIRS};

use terraswap::asset::{AssetInfo, PairInfo, PairInfoRaw};
use terraswap::factory::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, PairsResponse, QueryMsg,
};
use terraswap::hook::InitHook;
use terraswap::pair::InstantiateMsg as PairInstantiateMsg;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let config = Config {
        owner: deps.api.addr_canonicalize(info.sender.as_str())?,
        token_code_id: msg.token_code_id,
        pair_code_ids: msg.pair_code_ids,
    };

    CONFIG.save(deps.storage, &config)?;

    let mut messages: Vec<SubMsg> = vec![];
    if let Some(hook) = msg.init_hook {
        messages.push(SubMsg {
            msg: WasmMsg::Execute {
                contract_addr: hook.contract_addr,
                msg: hook.msg,
                funds: vec![],
            }
            .into(),
            id: 0,
            gas_limit: None,
            reply_on: ReplyOn::Never,
        });
    }

    Ok(Response {
        events: vec![],
        messages,
        attributes: vec![],
        data: None,
    })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::UpdateConfig {
            owner,
            token_code_id,
            pair_code_ids,
        } => execute_update_config(deps, env, info, owner, token_code_id, pair_code_ids),
        ExecuteMsg::CreatePair {
            pair_code_id,
            asset_infos,
            init_hook,
        } => execute_create_pair(deps, env, pair_code_id, asset_infos, init_hook),
        ExecuteMsg::Register { asset_infos } => register(deps, env, info, asset_infos),
    }
}

// Only owner can execute it
pub fn execute_update_config(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    owner: Option<String>,
    token_code_id: Option<u64>,
    pair_code_ids: Option<Vec<u64>>,
) -> Result<Response, ContractError> {
    let mut config: Config = CONFIG.load(deps.storage)?;

    // permission check
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(owner) = owner {
        // validate address format
        let _ = deps.api.addr_validate(&owner)?;

        config.owner = deps.api.addr_canonicalize(&owner)?;
    }

    if let Some(token_code_id) = token_code_id {
        config.token_code_id = token_code_id;
    }

    if let Some(pair_code_ids) = pair_code_ids {
        config.pair_code_ids = pair_code_ids;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response {
        events: vec![],
        messages: vec![],
        attributes: vec![attr("action", "update_config")],
        data: None,
    })
}

// Anyone can execute it to create swap pair
pub fn execute_create_pair(
    deps: DepsMut,
    env: Env,
    pair_code_id: u64,
    asset_infos: [AssetInfo; 2],
    init_hook: Option<InitHook>,
) -> Result<Response, ContractError> {
    let config: Config = CONFIG.load(deps.storage)?;
    let raw_infos = [
        asset_infos[0].to_raw(deps.api)?,
        asset_infos[1].to_raw(deps.api)?,
    ];

    if PAIRS
        .may_load(deps.storage, &pair_key(&raw_infos))
        .unwrap_or(None)
        .is_some()
    {
        return Err(StdError::generic_err("Pair already exists").into());
    }

    // Check if pair ID is whitelisted
    if !config.pair_code_ids.contains(&pair_code_id) {
        return Err(ContractError::PairCodeNotAllowed {});
    }

    PAIRS.save(
        deps.storage,
        &pair_key(&raw_infos),
        &PairInfoRaw {
            liquidity_token: CanonicalAddr::from(vec![]),
            contract_addr: CanonicalAddr::from(vec![]),
            asset_infos: raw_infos,
        },
    )?;

    let mut messages: Vec<SubMsg> = vec![SubMsg {
        msg: WasmMsg::Instantiate {
            code_id: pair_code_id,
            funds: vec![],
            admin: None,
            label: String::new(),
            msg: to_binary(&PairInstantiateMsg {
                asset_infos: asset_infos.clone(),
                token_code_id: config.token_code_id,
                init_hook: Some(InitHook {
                    contract_addr: env.contract.address.to_string(),
                    msg: to_binary(&ExecuteMsg::Register {
                        asset_infos: asset_infos.clone(),
                    })?,
                }),
            })?,
        }
        .into(),
        id: 0,
        gas_limit: None,
        reply_on: ReplyOn::Never,
    }];

    if let Some(hook) = init_hook {
        messages.push(SubMsg {
            msg: WasmMsg::Execute {
                contract_addr: hook.contract_addr,
                msg: hook.msg,
                funds: vec![],
            }
            .into(),
            id: 0,
            gas_limit: None,
            reply_on: ReplyOn::Never,
        });
    }

    Ok(Response {
        events: vec![],
        messages,
        attributes: vec![
            attr("action", "create_pair"),
            attr("pair", format!("{}-{}", asset_infos[0], asset_infos[1])),
        ],
        data: None,
    })
}

/// create pair execute this message
pub fn register(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    asset_infos: [AssetInfo; 2],
) -> Result<Response, ContractError> {
    let raw_infos = [
        asset_infos[0].to_raw(deps.api)?,
        asset_infos[1].to_raw(deps.api)?,
    ];
    let pair_info: PairInfoRaw = PAIRS.load(deps.storage, &pair_key(&raw_infos))?;
    if pair_info.contract_addr != CanonicalAddr::from(vec![]) {
        return Err(ContractError::PairWasRegistered {});
    }

    let pair_contract = info.sender;
    let liquidity_token = query_liquidity_token(deps.as_ref(), pair_contract.clone())?;
    PAIRS.save(
        deps.storage,
        &pair_key(&raw_infos),
        &PairInfoRaw {
            contract_addr: deps.api.addr_canonicalize(&pair_contract.to_string())?,
            liquidity_token: deps.api.addr_canonicalize(liquidity_token.as_str())?,
            ..pair_info
        },
    )?;

    Ok(Response {
        events: vec![],
        messages: vec![],
        attributes: vec![
            attr("action", "register"),
            attr("pair_contract_addr", pair_contract),
        ],
        data: None,
    })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::Pair { asset_infos } => to_binary(&query_pair(deps, asset_infos)?),
        QueryMsg::Pairs { start_after, limit } => {
            to_binary(&query_pairs(deps, start_after, limit)?)
        }
    }
}

pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let state: Config = CONFIG.load(deps.storage)?;
    let resp = ConfigResponse {
        owner: deps.api.addr_humanize(&state.owner)?.to_string(),
        token_code_id: state.token_code_id,
        pair_code_ids: state.pair_code_ids,
    };

    Ok(resp)
}

pub fn query_pair(deps: Deps, asset_infos: [AssetInfo; 2]) -> StdResult<PairInfo> {
    let raw_infos = [
        asset_infos[0].to_raw(deps.api)?,
        asset_infos[1].to_raw(deps.api)?,
    ];
    let pair_info: PairInfoRaw = PAIRS.load(deps.storage, &pair_key(&raw_infos))?;
    pair_info.to_normal(deps.api)
}

pub fn query_pairs(
    deps: Deps,
    start_after: Option<[AssetInfo; 2]>,
    limit: Option<u32>,
) -> StdResult<PairsResponse> {
    let start_after = if let Some(start_after) = start_after {
        Some([
            start_after[0].to_raw(deps.api)?,
            start_after[1].to_raw(deps.api)?,
        ])
    } else {
        None
    };

    let pairs: Vec<PairInfo> = read_pairs(deps, start_after, limit)?;
    let resp = PairsResponse { pairs };

    Ok(resp)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    Ok(Response::default())
}
