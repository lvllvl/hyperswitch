use std::borrow::Cow;

use common_utils::{
    ext_traits::{AsyncExt, ByteSliceExt, ValueExt},
    fp_utils, generate_id, pii,
};
// TODO : Evaluate all the helper functions ()
use error_stack::{report, IntoReport, ResultExt};
use josekit::jwe;
use masking::{ExposeInterface, PeekInterface};
use router_env::{instrument, tracing};
use storage_models::{enums, payment_intent};
use time::Duration;
use uuid::Uuid;

use super::{
    operations::{BoxedOperation, Operation, PaymentResponse},
    CustomerDetails, PaymentData,
};
use crate::{
    configs::settings::Server,
    consts,
    core::{
        errors::{self, CustomResult, RouterResult, StorageErrorExt},
        payment_methods::{cards, vault},
        payments,
    },
    db::StorageInterface,
    routes::{metrics, AppState},
    scheduler::{metrics as scheduler_metrics, workflows::payment_sync},
    services,
    types::{
        api::{self, admin, enums as api_enums, CustomerAcceptanceExt, MandateValidationFieldsExt},
        domain::{
            self,
            types::{self, AsyncLift},
        },
        storage::{self, enums as storage_enums, ephemeral_key},
        transformers::ForeignInto,
        ErrorResponse, RouterData,
    },
    utils::{
        self,
        crypto::{self, SignMessage},
        OptionExt,
    },
};

pub fn filter_mca_based_on_business_details(
    merchant_connector_accounts: Vec<domain::MerchantConnectorAccount>,
    payment_intent: Option<&storage_models::payment_intent::PaymentIntent>,
) -> Vec<domain::MerchantConnectorAccount> {
    if let Some(payment_intent) = payment_intent {
        merchant_connector_accounts
            .into_iter()
            .filter(|mca| {
                mca.business_country == payment_intent.business_country
                    && mca.business_label == payment_intent.business_label
            })
            .collect::<Vec<_>>()
    } else {
        merchant_connector_accounts
    }
}

pub async fn get_address_for_payment_request(
    db: &dyn StorageInterface,
    req_address: Option<&api::Address>,
    address_id: Option<&str>,
    merchant_id: &str,
    customer_id: &Option<String>,
) -> CustomResult<Option<domain::Address>, errors::ApiErrorResponse> {
    let key = types::get_merchant_enc_key(db, merchant_id.to_string())
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed while getting key for encryption")?;

    Ok(match req_address {
        Some(address) => {
            match address_id {
                Some(id) => {
                    let address_update = async {
                        Ok(storage::AddressUpdate::Update {
                            city: address
                                .address
                                .as_ref()
                                .and_then(|value| value.city.clone()),
                            country: address.address.as_ref().and_then(|value| value.country),
                            line1: address
                                .address
                                .as_ref()
                                .and_then(|value| value.line1.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            line2: address
                                .address
                                .as_ref()
                                .and_then(|value| value.line2.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            line3: address
                                .address
                                .as_ref()
                                .and_then(|value| value.line3.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            state: address
                                .address
                                .as_ref()
                                .and_then(|value| value.state.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            zip: address
                                .address
                                .as_ref()
                                .and_then(|value| value.zip.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            first_name: address
                                .address
                                .as_ref()
                                .and_then(|value| value.first_name.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            last_name: address
                                .address
                                .as_ref()
                                .and_then(|value| value.last_name.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            phone_number: address
                                .phone
                                .as_ref()
                                .and_then(|value| value.number.clone())
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            country_code: address
                                .phone
                                .as_ref()
                                .and_then(|value| value.country_code.clone()),
                        })
                    }
                    .await
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Failed while encrypting address")?;
                    Some(
                        db.update_address(id.to_owned(), address_update)
                            .await
                            .to_not_found_response(errors::ApiErrorResponse::AddressNotFound)?,
                    )
                }
                None => {
                    // generate a new address here
                    let customer_id = customer_id.as_deref().get_required_value("customer_id")?;

                    let address_details = address.address.clone().unwrap_or_default();
                    Some(
                        db.insert_address(
                            async {
                                Ok(domain::Address {
                                    phone_number: address
                                        .phone
                                        .as_ref()
                                        .and_then(|a| a.number.clone())
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    country_code: address
                                        .phone
                                        .as_ref()
                                        .and_then(|a| a.country_code.clone()),
                                    customer_id: customer_id.to_string(),
                                    merchant_id: merchant_id.to_string(),
                                    address_id: generate_id(consts::ID_LENGTH, "add"),
                                    city: address_details.city,
                                    country: address_details.country,
                                    line1: address_details
                                        .line1
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    line2: address_details
                                        .line2
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    line3: address_details
                                        .line3
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    id: None,
                                    state: address_details
                                        .state
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    created_at: common_utils::date_time::now(),
                                    first_name: address_details
                                        .first_name
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    last_name: address_details
                                        .last_name
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                    modified_at: common_utils::date_time::now(),
                                    zip: address_details
                                        .zip
                                        .async_lift(|inner| types::encrypt_optional(inner, &key))
                                        .await?,
                                })
                            }
                            .await
                            .change_context(errors::ApiErrorResponse::InternalServerError)
                            .attach_printable("Failed while encrypting address while insert")?,
                        )
                        .await
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Failed while inserting new address")?,
                    )
                }
            }
        }
        None => match address_id {
            Some(id) => Some(db.find_address(id).await)
                .transpose()
                .to_not_found_response(errors::ApiErrorResponse::AddressNotFound)?,
            None => None,
        },
    })
}

pub async fn get_address_by_id(
    db: &dyn StorageInterface,
    address_id: Option<String>,
) -> CustomResult<Option<domain::Address>, errors::ApiErrorResponse> {
    match address_id {
        None => Ok(None),
        Some(address_id) => Ok(db.find_address(&address_id).await.ok()),
    }
}

pub async fn get_token_pm_type_mandate_details(
    state: &AppState,
    request: &api::PaymentsRequest,
    mandate_type: Option<api::MandateTxnType>,
    merchant_account: &domain::MerchantAccount,
) -> RouterResult<(
    Option<String>,
    Option<storage_enums::PaymentMethod>,
    Option<api::MandateData>,
)> {
    match mandate_type {
        Some(api::MandateTxnType::NewMandateTxn) => {
            let setup_mandate = request
                .mandate_data
                .clone()
                .get_required_value("mandate_data")?;
            Ok((
                request.payment_token.to_owned(),
                request.payment_method.map(ForeignInto::foreign_into),
                Some(setup_mandate),
            ))
        }
        Some(api::MandateTxnType::RecurringMandateTxn) => {
            let (token_, payment_method_type_) =
                get_token_for_recurring_mandate(state, request, merchant_account).await?;
            Ok((token_, payment_method_type_, None))
        }
        None => Ok((
            request.payment_token.to_owned(),
            request.payment_method.map(ForeignInto::foreign_into),
            request.mandate_data.clone(),
        )),
    }
}

pub async fn get_token_for_recurring_mandate(
    state: &AppState,
    req: &api::PaymentsRequest,
    merchant_account: &domain::MerchantAccount,
) -> RouterResult<(Option<String>, Option<storage_enums::PaymentMethod>)> {
    let db = &*state.store;
    let mandate_id = req.mandate_id.clone().get_required_value("mandate_id")?;

    let mandate = db
        .find_mandate_by_merchant_id_mandate_id(&merchant_account.merchant_id, mandate_id.as_str())
        .await
        .to_not_found_response(errors::ApiErrorResponse::MandateNotFound)?;

    let customer = req.customer_id.clone().get_required_value("customer_id")?;

    let payment_method_id = {
        if mandate.customer_id != customer {
            Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                message: "customer_id must match mandate customer_id".into()
            }))?
        }
        if mandate.mandate_status != storage_enums::MandateStatus::Active {
            Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                message: "mandate is not active".into()
            }))?
        };
        mandate.payment_method_id.clone()
    };
    verify_mandate_details(
        req.amount.get_required_value("amount")?.into(),
        req.currency.get_required_value("currency")?,
        mandate.clone(),
    )?;

    let payment_method = db
        .find_payment_method(payment_method_id.as_str())
        .await
        .to_not_found_response(errors::ApiErrorResponse::PaymentMethodNotFound)?;

    let token = Uuid::new_v4().to_string();
    let locker_id = merchant_account
        .locker_id
        .to_owned()
        .get_required_value("locker_id")?;
    if let storage_models::enums::PaymentMethod::Card = payment_method.payment_method {
        let _ =
            cards::get_lookup_key_from_locker(state, &token, &payment_method, &locker_id).await?;
        if let Some(payment_method_from_request) = req.payment_method {
            let pm: storage_enums::PaymentMethod = payment_method_from_request.foreign_into();
            if pm != payment_method.payment_method {
                Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                    message:
                        "payment method in request does not match previously provided payment \
                                  method information"
                            .into()
                }))?
            }
        };

        Ok((Some(token), Some(payment_method.payment_method)))
    } else {
        Ok((None, Some(payment_method.payment_method)))
    }
}

#[instrument(skip_all)]
/// Check weather the merchant id in the request
/// and merchant id in the merchant account are same.
pub fn validate_merchant_id(
    merchant_id: &str,
    request_merchant_id: Option<&str>,
) -> CustomResult<(), errors::ApiErrorResponse> {
    // Get Merchant Id from the merchant
    // or get from merchant account

    let request_merchant_id = request_merchant_id.unwrap_or(merchant_id);

    utils::when(merchant_id.ne(request_merchant_id), || {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: format!(
                "Invalid `merchant_id`: {request_merchant_id} not found in merchant account"
            )
        }))
    })
}

#[instrument(skip_all)]
pub fn validate_request_amount_and_amount_to_capture(
    op_amount: Option<api::Amount>,
    op_amount_to_capture: Option<i64>,
) -> CustomResult<(), errors::ApiErrorResponse> {
    match (op_amount, op_amount_to_capture) {
        (None, _) => Ok(()),
        (Some(_amount), None) => Ok(()),
        (Some(amount), Some(amount_to_capture)) => {
            match amount {
                api::Amount::Value(amount_inner) => {
                    // If both amount and amount to capture is present
                    // then amount to be capture should be less than or equal to request amount
                    utils::when(!amount_to_capture.le(&amount_inner.get()), || {
                        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                            message: format!(
                            "amount_to_capture is greater than amount capture_amount: {amount_to_capture:?} request_amount: {amount:?}"
                        )
                        }))
                    })
                }
                api::Amount::Zero => {
                    // If the amount is Null but still amount_to_capture is passed this is invalid and
                    Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                        message: "amount_to_capture should not exist for when amount = 0"
                            .to_string()
                    }))
                }
            }
        }
    }
}

pub fn validate_mandate(
    req: impl Into<api::MandateValidationFields>,
    is_confirm_operation: bool,
) -> RouterResult<Option<api::MandateTxnType>> {
    let req: api::MandateValidationFields = req.into();
    match req.is_mandate() {
        Some(api::MandateTxnType::NewMandateTxn) => {
            validate_new_mandate_request(req, is_confirm_operation)?;
            Ok(Some(api::MandateTxnType::NewMandateTxn))
        }
        Some(api::MandateTxnType::RecurringMandateTxn) => {
            validate_recurring_mandate(req)?;
            Ok(Some(api::MandateTxnType::RecurringMandateTxn))
        }
        None => Ok(None),
    }
}

fn validate_new_mandate_request(
    req: api::MandateValidationFields,
    is_confirm_operation: bool,
) -> RouterResult<()> {
    // We need not check for customer_id in the confirm request if it is already passed
    //in create request

    fp_utils::when(!is_confirm_operation && req.customer_id.is_none(), || {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`customer_id` is mandatory for mandates".into()
        }))
    })?;

    let mandate_data = req
        .mandate_data
        .clone()
        .get_required_value("mandate_data")?;

    if api_enums::FutureUsage::OnSession
        == req
            .setup_future_usage
            .get_required_value("setup_future_usage")?
    {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`setup_future_usage` must be `off_session` for mandates".into()
        }))?
    };

    // Only use this validation if the customer_acceptance is present
    if mandate_data
        .customer_acceptance
        .map(|inner| inner.acceptance_type == api::AcceptanceType::Online && inner.online.is_none())
        .unwrap_or(false)
    {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`mandate_data.customer_acceptance.online` is required when \
                      `mandate_data.customer_acceptance.acceptance_type` is `online`"
                .into()
        }))?
    }

    let mandate_details = match mandate_data.mandate_type {
        Some(api_models::payments::MandateType::SingleUse(details)) => Some(details),
        Some(api_models::payments::MandateType::MultiUse(details)) => details,
        None => None,
    };
    mandate_details.and_then(|md| md.start_date.zip(md.end_date)).map(|(start_date, end_date)|
        utils::when (start_date >= end_date, || {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`mandate_data.mandate_type.{multi_use|single_use}.start_date` should be greater than  \
            `mandate_data.mandate_type.{multi_use|single_use}.end_date`"
                .into()
        }))
    })).transpose()?;

    Ok(())
}

pub fn validate_customer_id_mandatory_cases(
    has_shipping: bool,
    has_billing: bool,
    has_setup_future_usage: bool,
    customer_id: &Option<String>,
) -> RouterResult<()> {
    match (
        has_shipping,
        has_billing,
        has_setup_future_usage,
        customer_id,
    ) {
        (true, _, _, None) | (_, true, _, None) | (_, _, true, None) => {
            Err(errors::ApiErrorResponse::PreconditionFailed {
                message: "customer_id is mandatory when shipping or billing \
                address is given or when setup_future_usage is given"
                    .to_string(),
            })
            .into_report()
        }
        _ => Ok(()),
    }
}

pub fn create_startpay_url(
    server: &Server,
    payment_attempt: &storage::PaymentAttempt,
    payment_intent: &storage::PaymentIntent,
) -> String {
    format!(
        "{}/payments/redirect/{}/{}/{}",
        server.base_url,
        payment_intent.payment_id,
        payment_intent.merchant_id,
        payment_attempt.attempt_id
    )
}

pub fn create_redirect_url(
    router_base_url: &String,
    payment_attempt: &storage::PaymentAttempt,
    connector_name: &String,
    creds_identifier: Option<&str>,
) -> String {
    let creds_identifier_path = creds_identifier.map_or_else(String::new, |cd| format!("/{}", cd));
    format!(
        "{}/payments/{}/{}/redirect/response/{}",
        router_base_url, payment_attempt.payment_id, payment_attempt.merchant_id, connector_name,
    ) + &creds_identifier_path
}

pub fn create_webhook_url(
    router_base_url: &String,
    merchant_id: &String,
    connector_name: &String,
) -> String {
    format!(
        "{}/webhooks/{}/{}",
        router_base_url, merchant_id, connector_name
    )
}
pub fn create_complete_authorize_url(
    router_base_url: &String,
    payment_attempt: &storage::PaymentAttempt,
    connector_name: &String,
) -> String {
    format!(
        "{}/payments/{}/{}/redirect/complete/{}",
        router_base_url, payment_attempt.payment_id, payment_attempt.merchant_id, connector_name
    )
}

fn validate_recurring_mandate(req: api::MandateValidationFields) -> RouterResult<()> {
    req.mandate_id.check_value_present("mandate_id")?;

    req.customer_id.check_value_present("customer_id")?;

    let confirm = req.confirm.get_required_value("confirm")?;
    if !confirm {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`confirm` must be `true` for mandates".into()
        }))?
    }

    let off_session = req.off_session.get_required_value("off_session")?;
    if !off_session {
        Err(report!(errors::ApiErrorResponse::PreconditionFailed {
            message: "`off_session` should be `true` for mandates".into()
        }))?
    }

    Ok(())
}

pub fn verify_mandate_details(
    request_amount: i64,
    request_currency: api_enums::Currency,
    mandate: storage::Mandate,
) -> RouterResult<()> {
    match mandate.mandate_type {
        storage_enums::MandateType::SingleUse => utils::when(
            mandate
                .mandate_amount
                .map(|mandate_amount| request_amount > mandate_amount)
                .unwrap_or(true),
            || {
                Err(report!(errors::ApiErrorResponse::MandateValidationFailed {
                    reason: "request amount is greater than mandate amount".to_string()
                }))
            },
        ),
        storage::enums::MandateType::MultiUse => utils::when(
            mandate
                .mandate_amount
                .map(|mandate_amount| {
                    (mandate.amount_captured.unwrap_or(0) + request_amount) > mandate_amount
                })
                .unwrap_or(false),
            || {
                Err(report!(errors::ApiErrorResponse::MandateValidationFailed {
                    reason: "request amount is greater than mandate amount".to_string()
                }))
            },
        ),
    }?;
    utils::when(
        mandate
            .mandate_currency
            .map(|mandate_currency| mandate_currency != request_currency.foreign_into())
            .unwrap_or(false),
        || {
            Err(report!(errors::ApiErrorResponse::MandateValidationFailed {
                reason: "cross currency mandates not supported".to_string()
            }))
        },
    )
}

#[instrument(skip_all)]
pub fn payment_attempt_status_fsm(
    payment_method_data: &Option<api::PaymentMethodData>,
    confirm: Option<bool>,
) -> storage_enums::AttemptStatus {
    match payment_method_data {
        Some(_) => match confirm {
            Some(true) => storage_enums::AttemptStatus::Pending,
            _ => storage_enums::AttemptStatus::ConfirmationAwaited,
        },
        None => storage_enums::AttemptStatus::PaymentMethodAwaited,
    }
}

pub fn payment_intent_status_fsm(
    payment_method_data: &Option<api::PaymentMethodData>,
    confirm: Option<bool>,
) -> storage_enums::IntentStatus {
    match payment_method_data {
        Some(_) => match confirm {
            Some(true) => storage_enums::IntentStatus::RequiresCustomerAction,
            _ => storage_enums::IntentStatus::RequiresConfirmation,
        },
        None => storage_enums::IntentStatus::RequiresPaymentMethod,
    }
}

pub async fn add_domain_task_to_pt<Op>(
    operation: &Op,
    state: &AppState,
    payment_attempt: &storage::PaymentAttempt,
) -> CustomResult<(), errors::ApiErrorResponse>
where
    Op: std::fmt::Debug,
{
    if check_if_operation_confirm(operation) {
        let connector_name = payment_attempt
            .connector
            .clone()
            .ok_or(errors::ApiErrorResponse::InternalServerError)?;

        let schedule_time = payment_sync::get_sync_process_schedule_time(
            &*state.store,
            &connector_name,
            &payment_attempt.merchant_id,
            0,
        )
        .await
        .into_report()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed while getting process schedule time")?;

        match schedule_time {
            Some(stime) => {
                scheduler_metrics::TASKS_ADDED_COUNT.add(&metrics::CONTEXT, 1, &[]); // Metrics
                super::add_process_sync_task(&*state.store, payment_attempt, stime)
                    .await
                    .into_report()
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Failed while adding task to process tracker")
            }
            None => Ok(()),
        }
    } else {
        Ok(())
    }
}

pub fn response_operation<'a, F, R>() -> BoxedOperation<'a, F, R>
where
    F: Send + Clone,
    PaymentResponse: Operation<F, R>,
{
    Box::new(PaymentResponse)
}

#[instrument(skip_all)]
pub(crate) async fn get_payment_method_create_request(
    payment_method: Option<&api::PaymentMethodData>,
    payment_method_type: Option<storage_enums::PaymentMethod>,
    customer: &domain::Customer,
) -> RouterResult<api::PaymentMethodCreate> {
    match payment_method {
        Some(pm_data) => match payment_method_type {
            Some(payment_method_type) => match pm_data {
                api::PaymentMethodData::Card(card) => {
                    let card_detail = api::CardDetail {
                        card_number: card.card_number.clone(),
                        card_exp_month: card.card_exp_month.clone(),
                        card_exp_year: card.card_exp_year.clone(),
                        card_holder_name: Some(card.card_holder_name.clone()),
                    };
                    let customer_id = customer.customer_id.clone();
                    let payment_method_request = api::PaymentMethodCreate {
                        payment_method: payment_method_type.foreign_into(),
                        payment_method_type: None,
                        payment_method_issuer: card.card_issuer.clone(),
                        payment_method_issuer_code: None,
                        card: Some(card_detail),
                        metadata: None,
                        customer_id: Some(customer_id),
                        card_network: card
                            .card_network
                            .as_ref()
                            .map(|card_network| card_network.to_string()),
                    };
                    Ok(payment_method_request)
                }
                _ => {
                    let payment_method_request = api::PaymentMethodCreate {
                        payment_method: payment_method_type.foreign_into(),
                        payment_method_type: None,
                        payment_method_issuer: None,
                        payment_method_issuer_code: None,
                        card: None,
                        metadata: None,
                        customer_id: Some(customer.customer_id.to_owned()),
                        card_network: None,
                    };
                    Ok(payment_method_request)
                }
            },
            None => Err(report!(errors::ApiErrorResponse::MissingRequiredField {
                field_name: "payment_method_type"
            })
            .attach_printable("PaymentMethodType Required")),
        },
        None => Err(report!(errors::ApiErrorResponse::MissingRequiredField {
            field_name: "payment_method_data"
        })
        .attach_printable("PaymentMethodData required Or Card is already saved")),
    }
}

pub async fn get_customer_from_details<F: Clone>(
    db: &dyn StorageInterface,
    customer_id: Option<String>,
    merchant_id: &str,
    payment_data: &mut PaymentData<F>,
) -> CustomResult<Option<domain::Customer>, errors::StorageError> {
    match customer_id {
        None => Ok(None),
        Some(c_id) => {
            let customer = db
                .find_customer_optional_by_customer_id_merchant_id(&c_id, merchant_id)
                .await?;
            payment_data.email = payment_data.email.clone().or_else(|| {
                customer.as_ref().and_then(|inner| {
                    inner
                        .email
                        .clone()
                        .map(|encrypted_value| encrypted_value.into())
                })
            });
            Ok(customer)
        }
    }
}

pub async fn get_connector_default(
    _state: &AppState,
    request_connector: Option<serde_json::Value>,
) -> CustomResult<api::ConnectorChoice, errors::ApiErrorResponse> {
    Ok(request_connector.map_or(
        api::ConnectorChoice::Decide,
        api::ConnectorChoice::StraightThrough,
    ))
}

#[instrument(skip_all)]
pub async fn create_customer_if_not_exist<'a, F: Clone, R>(
    operation: BoxedOperation<'a, F, R>,
    db: &dyn StorageInterface,
    payment_data: &mut PaymentData<F>,
    req: Option<CustomerDetails>,
    merchant_id: &str,
) -> CustomResult<(BoxedOperation<'a, F, R>, Option<domain::Customer>), errors::StorageError> {
    let req = req
        .get_required_value("customer")
        .change_context(errors::StorageError::ValueNotFound("customer".to_owned()))?;

    let customer_id = req
        .customer_id
        .or(payment_data.payment_intent.customer_id.clone());

    let optional_customer = match customer_id {
        Some(customer_id) => {
            let customer_data = db
                .find_customer_optional_by_customer_id_merchant_id(&customer_id, merchant_id)
                .await?;
            Some(match customer_data {
                Some(c) => Ok(c),
                None => {
                    let key = types::get_merchant_enc_key(db, merchant_id.to_string()).await?;
                    let new_customer = async {
                        Ok(domain::Customer {
                            customer_id: customer_id.to_string(),
                            merchant_id: merchant_id.to_string(),
                            name: req
                                .name
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            email: req
                                .email
                                .clone()
                                .async_lift(|inner| {
                                    types::encrypt_optional(inner.map(|inner| inner.expose()), &key)
                                })
                                .await?,
                            phone: req
                                .phone
                                .clone()
                                .async_lift(|inner| types::encrypt_optional(inner, &key))
                                .await?,
                            phone_country_code: req.phone_country_code.clone(),
                            description: None,
                            created_at: common_utils::date_time::now(),
                            id: None,
                            metadata: None,
                            modified_at: common_utils::date_time::now(),
                            connector_customer: None,
                        })
                    }
                    .await
                    .change_context(errors::StorageError::SerializationFailed)
                    .attach_printable("Failed while encrypting Customer while insert")?;
                    metrics::CUSTOMER_CREATED.add(&metrics::CONTEXT, 1, &[]);
                    db.insert_customer(new_customer).await
                }
            })
        }
        None => match &payment_data.payment_intent.customer_id {
            None => None,
            Some(customer_id) => db
                .find_customer_optional_by_customer_id_merchant_id(customer_id, merchant_id)
                .await?
                .map(Ok),
        },
    };
    Ok((
        operation,
        match optional_customer {
            Some(customer) => {
                let customer = customer?;

                payment_data.payment_intent.customer_id = Some(customer.customer_id.clone());
                payment_data.email = payment_data.email.clone().or_else(|| {
                    customer
                        .email
                        .clone()
                        .map(|encrypted_value| encrypted_value.into())
                });

                Some(customer)
            }
            None => None,
        },
    ))
}

pub async fn make_pm_data<'a, F: Clone, R>(
    operation: BoxedOperation<'a, F, R>,
    state: &'a AppState,
    payment_data: &mut PaymentData<F>,
) -> RouterResult<(BoxedOperation<'a, F, R>, Option<api::PaymentMethodData>)> {
    let request = &payment_data.payment_method_data;
    let token = payment_data.token.clone();
    let hyperswitch_token = if let Some(token) = token {
        let redis_conn = state.store.get_redis_conn();
        let key = format!(
            "pm_token_{}_{}_hyperswitch",
            token,
            payment_data
                .payment_attempt
                .payment_method
                .to_owned()
                .get_required_value("payment_method")?,
        );

        let hyperswitch_token_option = redis_conn
            .get_key::<Option<String>>(&key)
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to fetch the token from redis")?;

        hyperswitch_token_option.or(Some(token))
    } else {
        None
    };

    let card_cvc = payment_data.card_cvc.clone();

    // TODO: Handle case where payment method and token both are present in request properly.
    let payment_method = match (request, hyperswitch_token) {
        (_, Some(hyperswitch_token)) => {
            let (pm, supplementary_data) = vault::Vault::get_payment_method_data_from_locker(
                state,
                &hyperswitch_token,
            )
            .await
            .attach_printable(
                "Payment method for given token not found or there was a problem fetching it",
            )?;

            utils::when(
                supplementary_data
                    .customer_id
                    .ne(&payment_data.payment_intent.customer_id),
                || {
                    Err(errors::ApiErrorResponse::PreconditionFailed { message: "customer associated with payment method and customer passed in payment are not same".into() })
                },
            )?;

            Ok::<_, error_stack::Report<errors::ApiErrorResponse>>(match pm.clone() {
                Some(api::PaymentMethodData::Card(card)) => {
                    payment_data.payment_attempt.payment_method =
                        Some(storage_enums::PaymentMethod::Card);
                    if let Some(cvc) = card_cvc {
                        let mut updated_card = card;
                        updated_card.card_cvc = cvc;
                        let updated_pm = api::PaymentMethodData::Card(updated_card);
                        vault::Vault::store_payment_method_data_in_locker(
                            state,
                            Some(hyperswitch_token),
                            &updated_pm,
                            payment_data.payment_intent.customer_id.to_owned(),
                            enums::PaymentMethod::Card,
                        )
                        .await?;
                        Some(updated_pm)
                    } else {
                        pm
                    }
                }

                Some(api::PaymentMethodData::Wallet(_)) => {
                    payment_data.payment_attempt.payment_method =
                        Some(storage_enums::PaymentMethod::Wallet);
                    pm
                }

                Some(api::PaymentMethodData::BankTransfer(_)) => {
                    payment_data.payment_attempt.payment_method =
                        Some(storage_enums::PaymentMethod::BankTransfer);
                    pm
                }
                Some(_) => Err(errors::ApiErrorResponse::InternalServerError)
                    .into_report()
                    .attach_printable(
                        "Payment method received from locker is unsupported by locker",
                    )?,

                None => None,
            })
        }
        (pm_opt @ Some(pm @ api::PaymentMethodData::Card(_)), _) => {
            let token = vault::Vault::store_payment_method_data_in_locker(
                state,
                None,
                pm,
                payment_data.payment_intent.customer_id.to_owned(),
                enums::PaymentMethod::Card,
            )
            .await?;
            payment_data.token = Some(token);
            Ok(pm_opt.to_owned())
        }
        (pm @ Some(api::PaymentMethodData::PayLater(_)), _) => Ok(pm.to_owned()),
        (pm @ Some(api::PaymentMethodData::BankRedirect(_)), _) => Ok(pm.to_owned()),
        (pm @ Some(api::PaymentMethodData::Crypto(_)), _) => Ok(pm.to_owned()),
        (pm @ Some(api::PaymentMethodData::BankDebit(_)), _) => Ok(pm.to_owned()),
        (pm_opt @ Some(pm @ api::PaymentMethodData::BankTransfer(_)), _) => {
            let token = vault::Vault::store_payment_method_data_in_locker(
                state,
                None,
                pm,
                payment_data.payment_intent.customer_id.to_owned(),
                enums::PaymentMethod::BankTransfer,
            )
            .await?;
            payment_data.token = Some(token);
            Ok(pm_opt.to_owned())
        }
        (pm_opt @ Some(pm @ api::PaymentMethodData::Wallet(_)), _) => {
            let token = vault::Vault::store_payment_method_data_in_locker(
                state,
                None,
                pm,
                payment_data.payment_intent.customer_id.to_owned(),
                enums::PaymentMethod::Wallet,
            )
            .await?;
            payment_data.token = Some(token);
            Ok(pm_opt.to_owned())
        }
        _ => Ok(None),
    }?;

    Ok((operation, payment_method))
}

#[instrument(skip_all)]
pub(crate) fn validate_capture_method(
    capture_method: storage_enums::CaptureMethod,
) -> RouterResult<()> {
    utils::when(
        capture_method == storage_enums::CaptureMethod::Automatic,
        || {
            Err(report!(errors::ApiErrorResponse::PaymentUnexpectedState {
                field_name: "capture_method".to_string(),
                current_flow: "captured".to_string(),
                current_value: capture_method.to_string(),
                states: "manual_single, manual_multiple, scheduled".to_string()
            }))
        },
    )
}

#[instrument(skip_all)]
pub(crate) fn validate_status(status: storage_enums::IntentStatus) -> RouterResult<()> {
    utils::when(
        status != storage_enums::IntentStatus::RequiresCapture,
        || {
            Err(report!(errors::ApiErrorResponse::PaymentUnexpectedState {
                field_name: "payment.status".to_string(),
                current_flow: "captured".to_string(),
                current_value: status.to_string(),
                states: "requires_capture".to_string()
            }))
        },
    )
}

#[instrument(skip_all)]
pub(crate) fn validate_amount_to_capture(
    amount: i64,
    amount_to_capture: Option<i64>,
) -> RouterResult<()> {
    utils::when(
        amount_to_capture.is_some() && (Some(amount) < amount_to_capture),
        || {
            Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                message: "amount_to_capture is greater than amount".to_string()
            }))
        },
    )
}

#[instrument(skip_all)]
pub(crate) fn validate_payment_method_fields_present(
    req: &api::PaymentsRequest,
) -> RouterResult<()> {
    utils::when(
        req.payment_method.is_none() && req.payment_method_data.is_some(),
        || {
            Err(errors::ApiErrorResponse::MissingRequiredField {
                field_name: "payment_method",
            })
        },
    )?;

    utils::when(
        req.payment_method.is_some()
            && req.payment_method_data.is_none()
            && req.payment_token.is_none(),
        || {
            Err(errors::ApiErrorResponse::MissingRequiredField {
                field_name: "payment_method_data",
            })
        },
    )?;

    Ok(())
}

pub fn check_force_psync_precondition(
    status: &storage_enums::AttemptStatus,
    connector_transaction_id: &Option<String>,
) -> bool {
    !matches!(
        status,
        storage_enums::AttemptStatus::Charged
            | storage_enums::AttemptStatus::AutoRefunded
            | storage_enums::AttemptStatus::Voided
            | storage_enums::AttemptStatus::CodInitiated
            | storage_enums::AttemptStatus::Authorized
            | storage_enums::AttemptStatus::Started
            | storage_enums::AttemptStatus::Failure
    ) && connector_transaction_id.is_some()
}

pub fn append_option<T, U, F, V>(func: F, option1: Option<T>, option2: Option<U>) -> Option<V>
where
    F: FnOnce(T, U) -> V,
{
    Some(func(option1?, option2?))
}

#[cfg(feature = "olap")]
pub(super) async fn filter_by_constraints(
    db: &dyn StorageInterface,
    constraints: &api::PaymentListConstraints,
    merchant_id: &str,
    storage_scheme: storage_enums::MerchantStorageScheme,
) -> CustomResult<Vec<storage::PaymentIntent>, errors::StorageError> {
    let result = db
        .filter_payment_intent_by_constraints(merchant_id, constraints, storage_scheme)
        .await?;
    Ok(result)
}

#[cfg(feature = "olap")]
pub(super) fn validate_payment_list_request(
    req: &api::PaymentListConstraints,
) -> CustomResult<(), errors::ApiErrorResponse> {
    utils::when(req.limit > 100 || req.limit < 1, || {
        Err(errors::ApiErrorResponse::InvalidRequestData {
            message: "limit should be in between 1 and 100".to_string(),
        })
    })?;
    Ok(())
}

pub fn get_handle_response_url(
    payment_id: String,
    merchant_account: &domain::MerchantAccount,
    response: api::PaymentsResponse,
    connector: String,
) -> RouterResult<api::RedirectionResponse> {
    let payments_return_url = response.return_url.as_ref();

    let redirection_response = make_pg_redirect_response(payment_id, &response, connector);

    let return_url = make_merchant_url_with_response(
        merchant_account,
        redirection_response,
        payments_return_url,
    )
    .attach_printable("Failed to make merchant url with response")?;

    make_url_with_signature(&return_url, merchant_account)
}

pub fn make_merchant_url_with_response(
    merchant_account: &domain::MerchantAccount,
    redirection_response: api::PgRedirectResponse,
    request_return_url: Option<&String>,
) -> RouterResult<String> {
    // take return url if provided in the request else use merchant return url
    let url = request_return_url
        .or(merchant_account.return_url.as_ref())
        .get_required_value("return_url")?;

    let status_check = redirection_response.status;

    let payment_intent_id = redirection_response.payment_id;

    let merchant_url_with_response = if merchant_account.redirect_to_merchant_with_http_post {
        url::Url::parse_with_params(
            url,
            &[
                ("status", status_check.to_string()),
                ("payment_intent_client_secret", payment_intent_id),
            ],
        )
        .into_report()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to parse the url with param")?
    } else {
        let amount = redirection_response.amount.get_required_value("amount")?;
        url::Url::parse_with_params(
            url,
            &[
                ("status", status_check.to_string()),
                ("payment_intent_client_secret", payment_intent_id),
                ("amount", amount.to_string()),
            ],
        )
        .into_report()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to parse the url with param")?
    };

    Ok(merchant_url_with_response.to_string())
}

pub async fn make_ephemeral_key(
    state: &AppState,
    customer_id: String,
    merchant_id: String,
) -> errors::RouterResponse<ephemeral_key::EphemeralKey> {
    let store = &state.store;
    let id = utils::generate_id(consts::ID_LENGTH, "eki");
    let secret = format!("epk_{}", &Uuid::new_v4().simple().to_string());
    let ek = ephemeral_key::EphemeralKeyNew {
        id,
        customer_id,
        merchant_id,
        secret,
    };
    let ek = store
        .create_ephemeral_key(ek, state.conf.eph_key.validity)
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to create ephemeral key")?;
    Ok(services::ApplicationResponse::Json(ek))
}

pub async fn delete_ephemeral_key(
    store: &dyn StorageInterface,
    ek_id: String,
) -> errors::RouterResponse<ephemeral_key::EphemeralKey> {
    let ek = store
        .delete_ephemeral_key(&ek_id)
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to delete ephemeral key")?;
    Ok(services::ApplicationResponse::Json(ek))
}

pub fn make_pg_redirect_response(
    payment_id: String,
    response: &api::PaymentsResponse,
    connector: String,
) -> api::PgRedirectResponse {
    api::PgRedirectResponse {
        payment_id,
        status: response.status,
        gateway_id: connector,
        customer_id: response.customer_id.to_owned(),
        amount: Some(response.amount),
    }
}

pub fn make_url_with_signature(
    redirect_url: &str,
    merchant_account: &domain::MerchantAccount,
) -> RouterResult<api::RedirectionResponse> {
    let mut url = url::Url::parse(redirect_url)
        .into_report()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to parse the url")?;

    let mut base_url = url.clone();
    base_url.query_pairs_mut().clear();

    let url = if merchant_account.enable_payment_response_hash {
        let key = merchant_account
            .payment_response_hash_key
            .as_ref()
            .get_required_value("payment_response_hash_key")?;
        let signature = hmac_sha512_sorted_query_params(
            &mut url.query_pairs().collect::<Vec<_>>(),
            key.as_str(),
        )?;

        url.query_pairs_mut()
            .append_pair("signature", &signature)
            .append_pair("signature_algorithm", "HMAC-SHA512");
        url.to_owned()
    } else {
        url.to_owned()
    };

    let parameters = url
        .query_pairs()
        .collect::<Vec<_>>()
        .iter()
        .map(|(key, value)| (key.clone().into_owned(), value.clone().into_owned()))
        .collect::<Vec<_>>();

    Ok(api::RedirectionResponse {
        return_url: base_url.to_string(),
        params: parameters,
        return_url_with_query_params: url.to_string(),
        http_method: if merchant_account.redirect_to_merchant_with_http_post {
            services::Method::Post.to_string()
        } else {
            services::Method::Get.to_string()
        },
        headers: Vec::new(),
    })
}

pub fn hmac_sha512_sorted_query_params(
    params: &mut [(Cow<'_, str>, Cow<'_, str>)],
    key: &str,
) -> RouterResult<String> {
    params.sort();
    let final_string = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");

    let signature = crypto::HmacSha512::sign_message(
        &crypto::HmacSha512,
        key.as_bytes(),
        final_string.as_bytes(),
    )
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Failed to sign the message")?;

    Ok(hex::encode(signature))
}

pub fn check_if_operation_confirm<Op: std::fmt::Debug>(operations: Op) -> bool {
    format!("{operations:?}") == "PaymentConfirm"
}

pub fn generate_mandate(
    merchant_id: String,
    connector: String,
    setup_mandate_details: Option<api::MandateData>,
    customer: &Option<domain::Customer>,
    payment_method_id: String,
    connector_mandate_id: Option<pii::SecretSerdeValue>,
    network_txn_id: Option<String>,
) -> CustomResult<Option<storage::MandateNew>, errors::ApiErrorResponse> {
    match (setup_mandate_details, customer) {
        (Some(data), Some(cus)) => {
            let mandate_id = utils::generate_id(consts::ID_LENGTH, "man");

            // The construction of the mandate new must be visible
            let mut new_mandate = storage::MandateNew::default();

            let customer_acceptance = data
                .customer_acceptance
                .get_required_value("customer_acceptance")?;
            new_mandate
                .set_mandate_id(mandate_id)
                .set_customer_id(cus.customer_id.clone())
                .set_merchant_id(merchant_id)
                .set_payment_method_id(payment_method_id)
                .set_connector(connector)
                .set_mandate_status(storage_enums::MandateStatus::Active)
                .set_connector_mandate_ids(connector_mandate_id)
                .set_network_transaction_id(network_txn_id)
                .set_customer_ip_address(
                    customer_acceptance
                        .get_ip_address()
                        .map(masking::Secret::new),
                )
                .set_customer_user_agent(customer_acceptance.get_user_agent())
                .set_customer_accepted_at(Some(customer_acceptance.get_accepted_at()));

            Ok(Some(
                match data.mandate_type.get_required_value("mandate_type")? {
                    api::MandateType::SingleUse(data) => new_mandate
                        .set_mandate_amount(Some(data.amount))
                        .set_mandate_currency(Some(data.currency.foreign_into()))
                        .set_mandate_type(storage_enums::MandateType::SingleUse)
                        .to_owned(),

                    api::MandateType::MultiUse(op_data) => match op_data {
                        Some(data) => new_mandate
                            .set_mandate_amount(Some(data.amount))
                            .set_mandate_currency(Some(data.currency.foreign_into()))
                            .set_start_date(data.start_date)
                            .set_end_date(data.end_date)
                            .set_metadata(data.metadata),
                        None => &mut new_mandate,
                    }
                    .set_mandate_type(storage_enums::MandateType::MultiUse)
                    .to_owned(),
                },
            ))
        }
        (_, _) => Ok(None),
    }
}

// A function to manually authenticate the client secret with intent fulfillment time
pub(crate) fn authenticate_client_secret(
    request_client_secret: Option<&String>,
    payment_intent: &payment_intent::PaymentIntent,
    merchant_intent_fulfillment_time: Option<i64>,
) -> Result<(), errors::ApiErrorResponse> {
    match (request_client_secret, &payment_intent.client_secret) {
        (Some(req_cs), Some(pi_cs)) => {
            if req_cs != pi_cs {
                Err(errors::ApiErrorResponse::ClientSecretInvalid)
            } else {
                //This is done to check whether the merchant_account's intent fulfillment time has expired or not
                let payment_intent_fulfillment_deadline =
                    payment_intent.created_at.saturating_add(Duration::seconds(
                        merchant_intent_fulfillment_time
                            .unwrap_or(consts::DEFAULT_FULFILLMENT_TIME),
                    ));
                let current_timestamp = common_utils::date_time::now();
                fp_utils::when(
                    current_timestamp > payment_intent_fulfillment_deadline,
                    || Err(errors::ApiErrorResponse::ClientSecretExpired),
                )
            }
        }
        // If there is no client in payment intent, then it has expired
        (Some(_), None) => Err(errors::ApiErrorResponse::ClientSecretExpired),
        _ => Ok(()),
    }
}

pub(crate) fn validate_payment_status_against_not_allowed_statuses(
    intent_status: &storage_enums::IntentStatus,
    not_allowed_statuses: &[storage_enums::IntentStatus],
    action: &'static str,
) -> Result<(), errors::ApiErrorResponse> {
    fp_utils::when(not_allowed_statuses.contains(intent_status), || {
        Err(errors::ApiErrorResponse::PreconditionFailed {
            message: format!(
                "You cannot {action} this payment because it has status {intent_status}",
            ),
        })
    })
}

pub(crate) fn validate_pm_or_token_given(
    payment_method: &Option<api_enums::PaymentMethod>,
    payment_method_data: &Option<api::PaymentMethodData>,
    payment_method_type: &Option<api_enums::PaymentMethodType>,
    mandate_type: &Option<api::MandateTxnType>,
    token: &Option<String>,
) -> Result<(), errors::ApiErrorResponse> {
    utils::when(
        !matches!(
            payment_method_type,
            Some(api_enums::PaymentMethodType::Paypal)
        ) && !matches!(mandate_type, Some(api::MandateTxnType::RecurringMandateTxn))
            && token.is_none()
            && (payment_method_data.is_none() || payment_method.is_none()),
        || {
            Err(errors::ApiErrorResponse::InvalidRequestData {
                message: "A payment token or payment method data is required".to_string(),
            })
        },
    )
}

// A function to perform database lookup and then verify the client secret
pub async fn verify_payment_intent_time_and_client_secret(
    db: &dyn StorageInterface,
    merchant_account: &domain::MerchantAccount,
    client_secret: Option<String>,
) -> error_stack::Result<Option<storage::PaymentIntent>, errors::ApiErrorResponse> {
    client_secret
        .async_map(|cs| async move {
            let payment_id = get_payment_id_from_client_secret(&cs);

            let payment_intent = db
                .find_payment_intent_by_payment_id_merchant_id(
                    &payment_id,
                    &merchant_account.merchant_id,
                    merchant_account.storage_scheme,
                )
                .await
                .change_context(errors::ApiErrorResponse::PaymentNotFound)?;

            authenticate_client_secret(
                Some(&cs),
                &payment_intent,
                merchant_account.intent_fulfillment_time,
            )?;
            Ok(payment_intent)
        })
        .await
        .transpose()
}

fn connector_needs_business_sub_label(connector_name: &str) -> bool {
    let connectors_list = [api_models::enums::Connector::Cybersource];
    connectors_list
        .map(|connector| connector.to_string())
        .contains(&connector_name.to_string())
}

/// Create the connector label
/// {connector_name}_{country}_{business_label}
pub fn get_connector_label(
    business_country: api_models::enums::CountryAlpha2,
    business_label: &str,
    business_sub_label: Option<&String>,
    connector_name: &str,
) -> String {
    let mut connector_label = format!("{connector_name}_{business_country}_{business_label}");

    // Business sub label is currently being used only for cybersource
    // To ensure backwards compatibality, cybersource mca's created before this change
    // will have the business_sub_label value as default.
    //
    // Even when creating the connector account, if no sub label is provided, default will be used
    if connector_needs_business_sub_label(connector_name) {
        if let Some(sub_label) = business_sub_label {
            connector_label.push_str(&format!("_{sub_label}"));
        } else {
            connector_label.push_str("_default"); // For backwards compatibality
        }
    }

    connector_label
}

/// Do lazy parsing of primary business details
/// If both country and label are passed, no need to parse business details from merchant_account
/// If any one is missing, get it from merchant_account
/// If there is more than one label or country configured in merchant account, then
/// passing business details for payment is mandatory to avoid ambiguity
pub fn get_business_details(
    business_country: Option<api_enums::CountryAlpha2>,
    business_label: Option<&String>,
    merchant_account: &domain::MerchantAccount,
) -> RouterResult<(api_enums::CountryAlpha2, String)> {
    let (business_country, business_label) = match business_country.zip(business_label) {
        Some((business_country, business_label)) => {
            (business_country.to_owned(), business_label.to_owned())
        }
        None => {
            // Parse the primary business details from merchant account
            let primary_business_details: Vec<api_models::admin::PrimaryBusinessDetails> =
                merchant_account
                    .primary_business_details
                    .clone()
                    .parse_value("PrimaryBusinessDetails")
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("failed to parse primary business details")?;

            if primary_business_details.len() == 1 {
                let primary_business_details = primary_business_details.first().ok_or(
                    errors::ApiErrorResponse::MissingRequiredField {
                        field_name: "primary_business_details",
                    },
                )?;
                (
                    business_country.unwrap_or_else(|| primary_business_details.country.to_owned()),
                    business_label
                        .map(ToString::to_string)
                        .unwrap_or_else(|| primary_business_details.business.to_owned()),
                )
            } else {
                // If primary business details are not present or more than one
                Err(report!(errors::ApiErrorResponse::MissingRequiredField {
                    field_name: "business_country, business_label"
                }))?
            }
        }
    };

    Ok((business_country, business_label))
}

#[inline]
pub(crate) fn get_payment_id_from_client_secret(cs: &str) -> String {
    cs.split('_').take(2).collect::<Vec<&str>>().join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authenticate_client_secret_fulfillment_time_not_expired() {
        let payment_intent = payment_intent::PaymentIntent {
            id: 21,
            payment_id: "23".to_string(),
            merchant_id: "22".to_string(),
            status: storage_enums::IntentStatus::RequiresCapture,
            amount: 200,
            currency: None,
            amount_captured: None,
            customer_id: None,
            description: None,
            return_url: None,
            metadata: None,
            connector_id: None,
            shipping_address_id: None,
            billing_address_id: None,
            statement_descriptor_name: None,
            statement_descriptor_suffix: None,
            created_at: common_utils::date_time::now(),
            modified_at: common_utils::date_time::now(),
            last_synced: None,
            setup_future_usage: None,
            off_session: None,
            client_secret: Some("1".to_string()),
            active_attempt_id: "nopes".to_string(),
            business_country: storage_enums::CountryAlpha2::AG,
            business_label: "no".to_string(),
        };
        let req_cs = Some("1".to_string());
        let merchant_fulfillment_time = Some(900);
        assert!(authenticate_client_secret(
            req_cs.as_ref(),
            &payment_intent,
            merchant_fulfillment_time
        )
        .is_ok()); // Check if the result is an Ok variant
    }

    #[test]
    fn test_authenticate_client_secret_fulfillment_time_expired() {
        let payment_intent = payment_intent::PaymentIntent {
            id: 21,
            payment_id: "23".to_string(),
            merchant_id: "22".to_string(),
            status: storage_enums::IntentStatus::RequiresCapture,
            amount: 200,
            currency: None,
            amount_captured: None,
            customer_id: None,
            description: None,
            return_url: None,
            metadata: None,
            connector_id: None,
            shipping_address_id: None,
            billing_address_id: None,
            statement_descriptor_name: None,
            statement_descriptor_suffix: None,
            created_at: common_utils::date_time::now().saturating_sub(Duration::seconds(20)),
            modified_at: common_utils::date_time::now(),
            last_synced: None,
            setup_future_usage: None,
            off_session: None,
            client_secret: Some("1".to_string()),
            active_attempt_id: "nopes".to_string(),
            business_country: storage_enums::CountryAlpha2::AG,
            business_label: "no".to_string(),
        };
        let req_cs = Some("1".to_string());
        let merchant_fulfillment_time = Some(10);
        assert!(authenticate_client_secret(
            req_cs.as_ref(),
            &payment_intent,
            merchant_fulfillment_time
        )
        .is_err())
    }

    #[test]
    fn test_authenticate_client_secret_expired() {
        let payment_intent = payment_intent::PaymentIntent {
            id: 21,
            payment_id: "23".to_string(),
            merchant_id: "22".to_string(),
            status: storage_enums::IntentStatus::RequiresCapture,
            amount: 200,
            currency: None,
            amount_captured: None,
            customer_id: None,
            description: None,
            return_url: None,
            metadata: None,
            connector_id: None,
            shipping_address_id: None,
            billing_address_id: None,
            statement_descriptor_name: None,
            statement_descriptor_suffix: None,
            created_at: common_utils::date_time::now().saturating_sub(Duration::seconds(20)),
            modified_at: common_utils::date_time::now(),
            last_synced: None,
            setup_future_usage: None,
            off_session: None,
            client_secret: None,
            active_attempt_id: "nopes".to_string(),
            business_country: storage_enums::CountryAlpha2::AG,
            business_label: "no".to_string(),
        };
        let req_cs = Some("1".to_string());
        let merchant_fulfillment_time = Some(10);
        assert!(authenticate_client_secret(
            req_cs.as_ref(),
            &payment_intent,
            merchant_fulfillment_time
        )
        .is_err())
    }
}

// This function will be removed after moving this functionality to server_wrap and using cache instead of config
pub async fn insert_merchant_connector_creds_to_config(
    db: &dyn StorageInterface,
    merchant_id: &str,
    merchant_connector_details: admin::MerchantConnectorDetailsWrap,
) -> RouterResult<()> {
    if let Some(encoded_data) = merchant_connector_details.encoded_data {
        match db
            .insert_config(storage::ConfigNew {
                key: format!(
                    "mcd_{merchant_id}_{}",
                    merchant_connector_details.creds_identifier
                ),
                config: encoded_data.peek().to_owned(),
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                if err.current_context().is_db_unique_violation() {
                    Ok(())
                } else {
                    Err(err
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Failed to insert connector_creds to config"))
                }
            }
        }
    } else {
        Ok(())
    }
}

pub enum MerchantConnectorAccountType {
    DbVal(domain::MerchantConnectorAccount),
    CacheVal(api_models::admin::MerchantConnectorDetails),
}

impl MerchantConnectorAccountType {
    pub fn get_metadata(&self) -> Option<masking::Secret<serde_json::Value>> {
        match self {
            Self::DbVal(val) => val.metadata.to_owned(),
            Self::CacheVal(val) => val.metadata.to_owned(),
        }
    }
    pub fn get_connector_account_details(&self) -> serde_json::Value {
        match self {
            Self::DbVal(val) => val.connector_account_details.peek().to_owned(),
            Self::CacheVal(val) => val.connector_account_details.peek().to_owned(),
        }
    }

    pub fn is_disabled(&self) -> bool {
        match self {
            Self::DbVal(ref inner) => inner.disabled.unwrap_or(false),
            // Cached merchant connector account, only contains the account details,
            // the merchant connector account must only be cached if it's not disabled
            Self::CacheVal(_) => false,
        }
    }
}

pub async fn get_merchant_connector_account(
    state: &AppState,
    merchant_id: &str,
    connector_label: &str,
    creds_identifier: Option<String>,
) -> RouterResult<MerchantConnectorAccountType> {
    let db = &*state.store;
    match creds_identifier {
        Some(creds_identifier) => {
            let mca_config = db
                .find_config_by_key(format!("mcd_{merchant_id}_{creds_identifier}").as_str())
                .await
                .to_not_found_response(
                    errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
                        id: connector_label.to_string(),
                    },
                )?;

            #[cfg(feature = "kms")]
            let private_key = state.kms_secrets.jwekey.peek().tunnel_private_key.clone();

            #[cfg(not(feature = "kms"))]
            let private_key = state.conf.jwekey.tunnel_private_key.to_owned();

            let decrypted_mca = services::decrypt_jwe(mca_config.config.as_str(), services::KeyIdCheck::SkipKeyIdCheck, private_key, jwe::RSA_OAEP_256)
                                     .await
                                     .change_context(errors::ApiErrorResponse::InternalServerError)
                                     .attach_printable(
                                        "Failed to decrypt merchant_connector_details sent in request and then put in cache",
                                    )?;

            let res = String::into_bytes(decrypted_mca)
                        .parse_struct("MerchantConnectorDetails")
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable(
                            "Failed to parse merchant_connector_details sent in request and then put in cache",
                        )?;

            Ok(MerchantConnectorAccountType::CacheVal(res))
        }
        None => db
            .find_merchant_connector_account_by_merchant_id_connector_label(
                merchant_id,
                connector_label,
            )
            .await
            .map(MerchantConnectorAccountType::DbVal)
            .change_context(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
                id: connector_label.to_string(),
            }),
    }
}

/// This function replaces the request and response type of routerdata with the
/// request and response type passed
/// # Arguments
///
/// * `router_data` - original router data
/// * `request` - new request
/// * `response` - new response
pub fn router_data_type_conversion<F1, F2, Req1, Req2, Res1, Res2>(
    router_data: RouterData<F1, Req1, Res1>,
    request: Req2,
    response: Result<Res2, ErrorResponse>,
) -> RouterData<F2, Req2, Res2> {
    RouterData {
        flow: std::marker::PhantomData,
        request,
        response,
        merchant_id: router_data.merchant_id,
        address: router_data.address,
        amount_captured: router_data.amount_captured,
        auth_type: router_data.auth_type,
        connector: router_data.connector,
        connector_auth_type: router_data.connector_auth_type,
        connector_meta_data: router_data.connector_meta_data,
        description: router_data.description,
        payment_id: router_data.payment_id,
        payment_method: router_data.payment_method,
        payment_method_id: router_data.payment_method_id,
        return_url: router_data.return_url,
        status: router_data.status,
        attempt_id: router_data.attempt_id,
        access_token: router_data.access_token,
        session_token: router_data.session_token,
        reference_id: None,
        payment_method_token: router_data.payment_method_token,
        customer_id: router_data.customer_id,
        connector_customer: router_data.connector_customer,
        preprocessing_id: router_data.preprocessing_id,
    }
}

pub fn get_attempt_type(
    payment_intent: &storage::PaymentIntent,
    payment_attempt: &storage::PaymentAttempt,
    request: &api::PaymentsRequest,
    action: &str,
) -> RouterResult<AttemptType> {
    match payment_intent.status {
        enums::IntentStatus::Failed => {
            if request.manual_retry {
                match payment_attempt.status {
                    enums::AttemptStatus::Started
                    | enums::AttemptStatus::AuthenticationPending
                    | enums::AttemptStatus::AuthenticationSuccessful
                    | enums::AttemptStatus::Authorized
                    | enums::AttemptStatus::Charged
                    | enums::AttemptStatus::Authorizing
                    | enums::AttemptStatus::CodInitiated
                    | enums::AttemptStatus::VoidInitiated
                    | enums::AttemptStatus::CaptureInitiated
                    | enums::AttemptStatus::Unresolved
                    | enums::AttemptStatus::Pending
                    | enums::AttemptStatus::ConfirmationAwaited
                    | enums::AttemptStatus::PartialCharged
                    | enums::AttemptStatus::Voided
                    | enums::AttemptStatus::AutoRefunded
                    | enums::AttemptStatus::PaymentMethodAwaited
                    | enums::AttemptStatus::DeviceDataCollectionPending => {
                        Err(errors::ApiErrorResponse::InternalServerError)
                            .into_report()
                            .attach_printable("Payment Attempt unexpected state")
                    }

                    storage_enums::AttemptStatus::VoidFailed
                    | storage_enums::AttemptStatus::RouterDeclined
                    | storage_enums::AttemptStatus::CaptureFailed =>  Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                        message:
                            format!("You cannot {action} this payment because it has status {}, and the previous attempt has the status {}", payment_intent.status, payment_attempt.status)
                        }
                    )),

                    storage_enums::AttemptStatus::AuthenticationFailed
                    | storage_enums::AttemptStatus::AuthorizationFailed
                    | storage_enums::AttemptStatus::Failure => Ok(AttemptType::New),
                }
            } else {
                Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                        message:
                            format!("You cannot {action} this payment because it has status {}, you can pass manual_retry as true in request to try this payment again", payment_intent.status)
                        }
                    ))
            }
        }
        enums::IntentStatus::Cancelled
        | enums::IntentStatus::RequiresCapture
        | enums::IntentStatus::Processing
        | enums::IntentStatus::Succeeded => {
            Err(report!(errors::ApiErrorResponse::PreconditionFailed {
                message: format!(
                    "You cannot {action} this payment because it has status {}",
                    payment_intent.status,
                ),
            }))
        }

        enums::IntentStatus::RequiresCustomerAction
        | enums::IntentStatus::RequiresMerchantAction
        | enums::IntentStatus::RequiresPaymentMethod
        | enums::IntentStatus::RequiresConfirmation => Ok(AttemptType::SameOld),
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum AttemptType {
    New,
    SameOld,
}

impl AttemptType {
    // The function creates a new payment_attempt from the previous payment attempt but doesn't populate fields like payment_method, error_code etc.
    // Logic to override the fields with data provided in the request should be done after this if required.
    // In case if fields are not overridden by the request then they contain the same data that was in the previous attempt provided it is populated in this function.
    #[inline(always)]
    fn make_new_payment_attempt(
        payment_method_data: &Option<api_models::payments::PaymentMethodData>,
        old_payment_attempt: storage::PaymentAttempt,
    ) -> storage::PaymentAttemptNew {
        let created_at @ modified_at @ last_synced = Some(common_utils::date_time::now());

        storage::PaymentAttemptNew {
            payment_id: old_payment_attempt.payment_id,
            merchant_id: old_payment_attempt.merchant_id,
            attempt_id: uuid::Uuid::new_v4().simple().to_string(),

            // A new payment attempt is getting created so, used the same function which is used to populate status in PaymentCreate Flow.
            status: payment_attempt_status_fsm(payment_method_data, Some(true)),

            amount: old_payment_attempt.amount,
            currency: old_payment_attempt.currency,
            save_to_locker: old_payment_attempt.save_to_locker,

            connector: None,

            error_message: None,
            offer_amount: old_payment_attempt.offer_amount,
            surcharge_amount: old_payment_attempt.surcharge_amount,
            tax_amount: old_payment_attempt.tax_amount,
            payment_method_id: None,
            payment_method: None,
            capture_method: old_payment_attempt.capture_method,
            capture_on: old_payment_attempt.capture_on,
            confirm: old_payment_attempt.confirm,
            authentication_type: old_payment_attempt.authentication_type,
            created_at,
            modified_at,
            last_synced,
            cancellation_reason: None,
            amount_to_capture: old_payment_attempt.amount_to_capture,

            // Once the payment_attempt is authorised then mandate_id is created. If this payment attempt is authorised then mandate_id will be overridden.
            // Since mandate_id is a contract between merchant and customer to debit customers amount adding it to newly created attempt
            mandate_id: old_payment_attempt.mandate_id,

            // The payment could be done from a different browser or same browser, it would probably be overridden by request data.
            browser_info: None,

            error_code: None,
            payment_token: None,
            connector_metadata: None,
            payment_experience: None,
            payment_method_type: None,
            payment_method_data: None,

            // In case it is passed in create and not in confirm,
            business_sub_label: old_payment_attempt.business_sub_label,
            // If the algorithm is entered in Create call from server side, it needs to be populated here, however it could be overridden from the request.
            straight_through_algorithm: old_payment_attempt.straight_through_algorithm,
            mandate_details: old_payment_attempt.mandate_details,
            preprocessing_step_id: None,
        }
    }

    pub async fn modify_payment_intent_and_payment_attempt(
        &self,
        request: &api::PaymentsRequest,
        fetched_payment_intent: storage::PaymentIntent,
        fetched_payment_attempt: storage::PaymentAttempt,
        db: &dyn StorageInterface,
        storage_scheme: storage::enums::MerchantStorageScheme,
    ) -> RouterResult<(storage::PaymentIntent, storage::PaymentAttempt)> {
        match self {
            Self::SameOld => Ok((fetched_payment_intent, fetched_payment_attempt)),
            Self::New => {
                let new_payment_attempt = db
                    .insert_payment_attempt(
                        Self::make_new_payment_attempt(
                            &request.payment_method_data,
                            fetched_payment_attempt,
                        ),
                        storage_scheme,
                    )
                    .await
                    .to_duplicate_response(errors::ApiErrorResponse::DuplicatePayment {
                        payment_id: fetched_payment_intent.payment_id.to_owned(),
                    })?;

                let updated_payment_intent = db
                    .update_payment_intent(
                        fetched_payment_intent,
                        storage::PaymentIntentUpdate::StatusAndAttemptUpdate {
                            status: payment_intent_status_fsm(
                                &request.payment_method_data,
                                Some(true),
                            ),
                            active_attempt_id: new_payment_attempt.attempt_id.to_owned(),
                        },
                        storage_scheme,
                    )
                    .await
                    .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

                Ok((updated_payment_intent, new_payment_attempt))
            }
        }
    }

    pub async fn get_connector_response(
        &self,
        payment_attempt: &storage::PaymentAttempt,
        db: &dyn StorageInterface,
        storage_scheme: storage::enums::MerchantStorageScheme,
    ) -> RouterResult<storage::ConnectorResponse> {
        match self {
            Self::New => db
                .insert_connector_response(
                    payments::PaymentCreate::make_connector_response(payment_attempt),
                    storage_scheme,
                )
                .await
                .to_duplicate_response(errors::ApiErrorResponse::DuplicatePayment {
                    payment_id: payment_attempt.payment_id.clone(),
                }),
            Self::SameOld => db
                .find_connector_response_by_payment_id_merchant_id_attempt_id(
                    &payment_attempt.payment_id,
                    &payment_attempt.merchant_id,
                    &payment_attempt.attempt_id,
                    storage_scheme,
                )
                .await
                .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound),
        }
    }
}
