use hdk::prelude::*;
use crate::change_rule::error::Error;
use crate::change_rule::entry::ChangeRule;
use crate::keyset_root::entry::KeysetRoot;
use crate::validate::ResolvedDependency;
use crate::validate::resolve_dependency;
use crate::device_authorization::device_invite_acceptance::entry::DeviceInviteAcceptance;

fn _validate_keyset_leaf(validate_data: &ValidateData, change_rule: &ChangeRule) -> ExternResult<ValidateCallbackResult> {
    // System validation should be catching this...
    let prev_header: HeaderHash = match validate_data.element.header().prev_header() {
        Some(prev_header) => prev_header.clone(),
        None => return Error::MissingPrevHeader.into(),
    };

    let leaf_header_element: Element = match get(change_rule.as_keyset_leaf_ref().clone(), GetOptions::content())? {
        Some(element) => element,
        None => return Ok(ValidateCallbackResult::UnresolvedDependencies(vec![change_rule.as_keyset_leaf_ref().clone().into()])),
    };

    // The leaf MUST be a device acceptance if not the root itself.
    if change_rule.keyset_root != change_rule.keyset_leaf {
        // so it MUST deserialize cleanly
        let device_invite_acceptance = match DeviceInviteAcceptance::try_from(&leaf_header_element) {
            Ok(device_invite_acceptance) => device_invite_acceptance,
            Err(e) => return Ok(ValidateCallbackResult::Invalid(e.to_string())),
        };
        // and the keyset root MUST be the same on the leaf and change rule
        if change_rule.as_keyset_root_ref() != device_invite_acceptance.as_keyset_root_authority_ref() {
            return Error::BadKeysetLeaf.into();
        }
    }
    else {
        // We already validate the keyset root as a root elsewhere so nothing to do here...
    }

    let leaf_query = ChainQueryFilter::default()
        // Inclusive start exclusive end.
        .sequence_range(leaf_header_element.header().header_seq()..validate_data.element.header().header_seq())
        // Not possible to have more than one KeysetRoot in a single chain so we only need to check for newer DeviceInviteAcceptance.
        .entry_type(entry_type!(DeviceInviteAcceptance)?);
    let leaf_agent_activity = get_agent_activity(validate_data.element.header().author().clone().into(), leaf_query, ActivityRequest::Full)?;

    let valid_top: u32 = match leaf_agent_activity.status {
        ChainStatus::Valid(ChainHead { header_seq, .. }) => header_seq,
        ChainStatus::Empty => return Ok(ValidateCallbackResult::UnresolvedDependencies(vec![prev_header.into()])),
        // Forked or Invalid
        // @todo is this deterministic when combined with a range filter?
        // i.e. if it is invalid or forked later does this change validation?
        _ => return Error::InvalidChain.into(),
    };

    // The agent activity needs to have validated the headers of the chain up to the point of this element.
    if valid_top < (validate_data.element.header().header_seq() - 1 ) {
        // This is a bit weird.
        // It is the _agent activity neighbour_ that has unresolved dependencies from the perspective of the _element authority_.
        return Ok(
            ValidateCallbackResult::UnresolvedDependencies(
                vec![prev_header.into()]
            )
        );
    }

    if leaf_agent_activity.valid_activity.len() != 1 {
        return Error::StaleKeysetLeaf.into();
    }
    if leaf_agent_activity.valid_activity[0].1 != change_rule.keyset_leaf {
        return Error::StaleKeysetLeaf.into();
    }

    Ok(ValidateCallbackResult::Valid)
}


fn _validate_spec(change_rule: &ChangeRule) -> ExternResult<ValidateCallbackResult> {
    if change_rule.spec_change.new_spec.sigs_required as usize > change_rule.spec_change.new_spec.authorized_signers.len() {
        Error::NotEnoughSigners.into()
    }
    else if change_rule.spec_change.new_spec.sigs_required < 1 {
            Error::NotEnoughSignatures.into()
    }
    else {
        Ok(ValidateCallbackResult::Valid)
    }
}

fn _validate_create_keyset_root(validate_data: &ValidateData, _: &ChangeRule, keyset_root: &KeysetRoot) -> ExternResult<ValidateCallbackResult> {
    // // The KSR needs to reference the author as the FDA.
    if keyset_root.as_first_deepkey_agent_ref() != validate_data.element.signed_header().header().author() {
        Error::AuthorNotFda.into()
    }
    else {
        Ok(ValidateCallbackResult::Valid)
    }
}

fn _validate_create_authorization(_: &ValidateData, change_rule: &ChangeRule, keyset_root: &KeysetRoot) -> ExternResult<ValidateCallbackResult> {
    // Signed by throwaway RootKey on Create, must have exactly one signature.
    if change_rule.as_spec_change_ref().as_authorization_of_new_spec_ref().len() > 1 {
        return Error::MultipleCreateSignatures.into();
    }
    let authorization_signature = match change_rule.as_spec_change_ref().as_authorization_of_new_spec_ref().iter().next() {
        Some(signature) => &signature.1,
        None => return Error::NoCreateSignature.into(),
    };

    // The signature must be valid.
    if verify_signature(
        keyset_root.as_root_pub_key_ref().clone(),
        authorization_signature.clone(),
        change_rule.as_spec_change_ref().as_new_spec_ref()
    )? {
        Ok(ValidateCallbackResult::Valid)
    } else {
        Error::BadCreateSignature.into()
    }
}

#[hdk_extern]
fn validate_create_entry_key_change_rule(validate_data: ValidateData) -> ExternResult<ValidateCallbackResult> {
    let proposed_change_rule = match ChangeRule::try_from(&validate_data.element) {
        Ok(change_rule) => change_rule,
        Err(e) => return Ok(ValidateCallbackResult::Invalid(e.to_string())),
    };

    let keyset_root: KeysetRoot = match resolve_dependency(proposed_change_rule.as_keyset_root_ref().clone().into())? {
        Ok(ResolvedDependency(_, keyset_root)) => keyset_root,
        Err(validate_callback_result) => return Ok(validate_callback_result),
    };

    match _validate_keyset_leaf(&validate_data, &proposed_change_rule) {
        Ok(ValidateCallbackResult::Valid) => { },
        validate_callback_result => return validate_callback_result,
    }

    match _validate_create_keyset_root(&validate_data, &proposed_change_rule, &keyset_root) {
        Ok(ValidateCallbackResult::Valid) => { },
        validate_callback_result => return validate_callback_result,
    }

    match _validate_create_authorization(&validate_data, &proposed_change_rule, &keyset_root) {
        Ok(ValidateCallbackResult::Valid) => { },
        validate_callback_result => return validate_callback_result,
    }

    match _validate_spec(&proposed_change_rule) {
        Ok(ValidateCallbackResult::Valid) => { },
        validate_callback_result => return validate_callback_result,
    }

    Ok(ValidateCallbackResult::Valid)
}

fn _validate_update_keyset_root(_: &ValidateData, previous_change_rule: &ChangeRule, proposed_change_rule: &ChangeRule) -> ExternResult<ValidateCallbackResult> {
    // The keyset root needs to be the same
    if proposed_change_rule.as_keyset_root_ref() != previous_change_rule.as_keyset_root_ref() {
        Error::KeysetRootMismatch.into()
    }
    else {
        Ok(ValidateCallbackResult::Valid)
    }
}

fn _validate_update_authorization(_: &ValidateData, previous_change_rule: &ChangeRule, proposed_change_rule: &ChangeRule) -> ExternResult<ValidateCallbackResult> {
    match previous_change_rule.authorize(&proposed_change_rule.spec_change.authorization_of_new_spec, &holochain_serialized_bytes::encode(&proposed_change_rule.spec_change.new_spec)?) {
        Ok(_) => Ok(ValidateCallbackResult::Valid),
        Err(e) => e.into(),
    }
}

fn _validate_update_spec(previous_change_rule: &ChangeRule, proposed_change_rule: &ChangeRule) -> ExternResult<ValidateCallbackResult> {
    if previous_change_rule.spec_change.new_spec == proposed_change_rule.spec_change.new_spec {
        Error::IdenticalUpdate.into()
    }
    else {
        Ok(ValidateCallbackResult::Valid)
    }
}

#[hdk_extern]
fn validate_update_entry_key_change_rule(validate_data: ValidateData) -> ExternResult<ValidateCallbackResult> {
    let proposed_change_rule = match ChangeRule::try_from(&validate_data.element) {
        Ok(change_rule) => change_rule,
        Err(e) => return e.into(),
    };

    // KeysetRoot needs to exist.
    match resolve_dependency::<KeysetRoot>(proposed_change_rule.as_keyset_root_ref().clone().into())? {
        Err(validate_callback_result) => return Ok(validate_callback_result),
        _ => { },
    }

    // On update we need to validate the proposed change rule under the rules of the previous rule.
    if let Header::Update(update_header) = validate_data.element.header().clone() {
        let previous_change_rule: ChangeRule = match resolve_dependency(update_header.original_header_address.into())? {
            Ok(ResolvedDependency(_, change_rule)) => change_rule,
            Err(validate_callback_result) => return Ok(validate_callback_result),
        };

        // Do all the new signers exist?
        for agent in proposed_change_rule.spec_change.new_spec.authorized_signers.iter() {
            match resolve_dependency::<AgentPubKey>(agent.to_owned().into())? {
                Err(validate_callback_result) => return Ok(validate_callback_result),
                _ => { },
            }
        }

        match _validate_keyset_leaf(&validate_data, &proposed_change_rule) {
            Ok(ValidateCallbackResult::Valid) => { },
            validate_callback_result => return validate_callback_result,
        }

        match _validate_update_keyset_root(&validate_data, &previous_change_rule, &proposed_change_rule) {
            Ok(ValidateCallbackResult::Valid) => { },
            validate_callback_result => return validate_callback_result,
        }

        match _validate_update_authorization(&validate_data, &previous_change_rule, &proposed_change_rule) {
            Ok(ValidateCallbackResult::Valid) => { },
            validate_callback_result => return validate_callback_result,
        }

        match _validate_update_spec(&previous_change_rule, &proposed_change_rule) {
            Ok(ValidateCallbackResult::Valid) => { },
            validate_callback_result => return validate_callback_result,
        }

        match _validate_spec(&proposed_change_rule) {
            Ok(ValidateCallbackResult::Valid) => { },
            validate_callback_result => return validate_callback_result,
        }

        Ok(ValidateCallbackResult::Valid)
    } else {
        // Holochain sent a non-update to an update validation.
        unreachable!();
    }
}

#[hdk_extern]
fn validate_delete_entry_key_change_rule(_: ValidateData) -> ExternResult<ValidateCallbackResult> {
    Error::DeleteAttempted.into()
}

#[cfg(test)]
pub mod tests {
    use hdk::prelude::*;
    use ::fixt::prelude::*;
    use holochain_types::prelude::*;
    use crate::change_rule::entry::ChangeRuleFixturator;
    use crate::keyset_root::entry::KeysetRootFixturator;
    use crate::change_rule::error::Error;
    use crate::change_rule::entry::AuthorizationFixturator;
    use crate::change_rule::entry::Authorization;
    use crate::device_authorization::device_invite_acceptance::entry::DeviceInviteAcceptanceFixturator;

    #[test]
    fn test_validate_keyset_leaf() {
        let mut validate_data = fixt!(ValidateData);
        let mut validate_header = fixt!(Update);
        validate_header.header_seq = 50;

        *validate_data.element.as_header_mut() = Header::Update(validate_header);

        let change_rule = fixt!(ChangeRule);
        let mut device_invite_acceptance = fixt!(DeviceInviteAcceptance);
        let mut device_invite_acceptance_element = fixt!(Element);

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(None));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Ok(ValidateCallbackResult::UnresolvedDependencies(vec![change_rule.as_keyset_leaf_ref().clone().into()])),
        );

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        set_hdk(mock_hdk);

        *device_invite_acceptance_element.as_header_mut() = Header::Update(fixt!(Update));
        *device_invite_acceptance_element.as_entry_mut() = ElementEntry::Present(device_invite_acceptance.clone().try_into().unwrap());

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            crate::error::Error::WrongHeader.into(),
        );

        let mut device_invite_element_header = fixt!(Create);
        device_invite_element_header.header_seq = 25;
        *device_invite_acceptance_element.as_header_mut() = Header::Create(device_invite_element_header);

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Error::BadKeysetLeaf.into(),
        );

        device_invite_acceptance.keyset_root_authority = change_rule.as_keyset_root_ref().clone();
        *device_invite_acceptance_element.as_entry_mut() = ElementEntry::Present(device_invite_acceptance.clone().try_into().unwrap());

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        mock_hdk.expect_zome_info().return_const(Ok(fixt!(ZomeInfo)));

        let mut agent_activity = AgentActivity {
            valid_activity: vec![],
            rejected_activity: vec![],
            status: ChainStatus::Empty,
            highest_observed: None,
            warrants: vec![],
        };

        mock_hdk.expect_get_agent_activity().return_const(Ok(agent_activity.clone()));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Ok(ValidateCallbackResult::UnresolvedDependencies(vec![validate_data.element.header().prev_header().unwrap().clone().into()]))
        );

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        mock_hdk.expect_zome_info().return_const(Ok(fixt!(ZomeInfo)));

        agent_activity.status = ChainStatus::Valid(ChainHead {
            hash: fixt!(HeaderHash),
            header_seq: 20
        });

        mock_hdk.expect_get_agent_activity().return_const(Ok(agent_activity.clone()));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Ok(ValidateCallbackResult::UnresolvedDependencies(vec![validate_data.element.header().prev_header().unwrap().clone().into()]))
        );

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        mock_hdk.expect_zome_info().return_const(Ok(fixt!(ZomeInfo)));

        agent_activity.status = ChainStatus::Valid(ChainHead {
            hash: fixt!(HeaderHash),
            header_seq: 200
        });

        agent_activity.valid_activity = vec![(0, fixt!(HeaderHash)), (30, fixt!(HeaderHash))];

        mock_hdk.expect_get_agent_activity().return_const(Ok(agent_activity.clone()));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Error::StaleKeysetLeaf.into(),
        );

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        mock_hdk.expect_zome_info().return_const(Ok(fixt!(ZomeInfo)));

        agent_activity.status = ChainStatus::Valid(ChainHead {
            hash: fixt!(HeaderHash),
            header_seq: 200
        });

        agent_activity.valid_activity = vec![(30, fixt!(HeaderHash))];

        mock_hdk.expect_get_agent_activity().return_const(Ok(agent_activity.clone()));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Error::StaleKeysetLeaf.into(),
        );

        let mut mock_hdk = MockHdkT::new();

        mock_hdk.expect_get().with(
            mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_leaf_ref().clone().into(),
                    GetOptions::content()
                )
            )
        )
        .return_const(Ok(Some(device_invite_acceptance_element.clone())));

        mock_hdk.expect_zome_info().return_const(Ok(fixt!(ZomeInfo)));

        agent_activity.status = ChainStatus::Valid(ChainHead {
            hash: fixt!(HeaderHash),
            header_seq: 200
        });

        agent_activity.valid_activity = vec![(30, change_rule.as_keyset_leaf_ref().clone())];

        mock_hdk.expect_get_agent_activity().return_const(Ok(agent_activity.clone()));

        set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_keyset_leaf(&validate_data, &change_rule),
            Ok(ValidateCallbackResult::Valid)
        );
    }

    #[test]
    fn test_validate_update() {
        // Random garbage won't have a valid ChangeRule on it.
        assert_eq!(
            super::validate_update_entry_key_change_rule(fixt!(ValidateData)),
            Ok(ValidateCallbackResult::Invalid("Element missing its ChangeRule".to_string())),
        );

        let mut validate_data = fixt!(ValidateData);
        let mut change_rule = fixt!(ChangeRule);
        // Ensure at least one signer.
        change_rule.spec_change.new_spec.authorized_signers.push(fixt!(AgentPubKey));

        let update_header = fixt!(Update);
        *validate_data.element.as_header_mut() = Header::Update(update_header.clone());

        let mut keyset_root_element = fixt!(Element);
        let keyset_root = fixt!(KeysetRoot);
        *keyset_root_element.as_entry_mut() = ElementEntry::Present(keyset_root.clone().try_into().unwrap());

        *validate_data.element.as_entry_mut() = ElementEntry::Present(change_rule.clone().try_into().unwrap());

        let previous_change_rule = fixt!(ChangeRule);
        let mut previous_element = fixt!(Element);
        *previous_element.as_entry_mut() = ElementEntry::Present(previous_change_rule.clone().try_into().unwrap());

        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_get()
        .with(mockall::predicate::eq(
            GetInput::new(
                change_rule.as_keyset_root_ref().clone().into(),
                GetOptions::content()
            )
        ))
        .times(1)
        .return_const(Ok(None));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            Ok(
                ValidateCallbackResult::UnresolvedDependencies(vec![change_rule.as_keyset_root_ref().clone().into()])
            ),
            super::validate_update_entry_key_change_rule(validate_data.clone()),
        );

        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_root_ref().clone().into(),
                    GetOptions::content()
                )
            ))
            .times(1)
            .return_const(Ok(Some(keyset_root_element.clone())));

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    update_header.original_header_address.clone().into(),
                    GetOptions::content(),
                )
            ))
            .times(1)
            .return_const(Ok(None));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            Ok(
                ValidateCallbackResult::UnresolvedDependencies(vec![update_header.original_header_address.clone().into()])
            ),
            super::validate_update_entry_key_change_rule(validate_data.clone()),
        );

        // New signers need to exist.
        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_root_ref().clone().into(),
                    GetOptions::content()
                )
            ))
            .times(1)
            .return_const(Ok(Some(keyset_root_element)));

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    update_header.original_header_address.clone().into(),
                    GetOptions::content(),
                )
            ))
            .times(1)
            .return_const(Ok(Some(previous_element)));

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    change_rule.spec_change.new_spec.authorized_signers[0].clone().into(),
                    GetOptions::content(),
                )
            ))
            .times(1)
            .return_const(Ok(None));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            Ok(ValidateCallbackResult::UnresolvedDependencies(vec![change_rule.spec_change.new_spec.authorized_signers[0].clone().into()])),
            super::validate_update_entry_key_change_rule(validate_data),
        );
    }

    #[test]
    fn test_validate_delete() {
        assert_eq!(
            super::validate_delete_entry_key_change_rule(fixt!(ValidateData)),
            Error::DeleteAttempted.into(),
        );
    }

    #[test]
    fn test_validate_create() {
        // Random garbage won't have a valid ChangeRule on it.
        assert_eq!(
            Ok(ValidateCallbackResult::Invalid("Element missing its ChangeRule".to_string())),
            super::validate_create_entry_key_change_rule(fixt!(ValidateData)),
        );

        let mut validate_data = fixt!(ValidateData);
        let change_rule = fixt!(ChangeRule);

        *validate_data.element.as_entry_mut() = ElementEntry::Present(change_rule.clone().try_into().unwrap());

        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_get()
            .with(mockall::predicate::eq(
                GetInput::new(
                    change_rule.as_keyset_root_ref().clone().into(),
                    GetOptions::content()
                )
            ))
            .times(1)
            .return_const(Ok(None));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            Ok(
                ValidateCallbackResult::UnresolvedDependencies(vec![change_rule.as_keyset_root_ref().clone().into()])
            ),
            super::validate_create_entry_key_change_rule(validate_data),
        );
    }

    #[test]
    fn test_validate_spec() {
        let mut change_rule = fixt!(ChangeRule);

        change_rule.spec_change.new_spec.sigs_required = 200;

        assert_eq!(
            super::_validate_spec(&change_rule),
            Error::NotEnoughSigners.into(),
        );

        change_rule.spec_change.new_spec.sigs_required = 0;

        assert_eq!(
            super::_validate_spec(&change_rule),
            Error::NotEnoughSignatures.into(),
        );

        change_rule.spec_change.new_spec.authorized_signers.push(fixt!(AgentPubKey));
        change_rule.spec_change.new_spec.sigs_required = change_rule.spec_change.new_spec.authorized_signers.len() as u8;
        assert_eq!(
            super::_validate_spec(&change_rule),
            Ok(ValidateCallbackResult::Valid),
        );
    }

    #[test]
    fn test_validate_create_keyset_root() {
        let mut validate_data = fixt!(ValidateData);
        let change_rule = fixt!(ChangeRule);
        let keyset_root = fixt!(KeysetRoot);
        let mut create_header = fixt!(Create);

        *validate_data.element.as_header_mut() = Header::Create(create_header.clone());

        // The FDA cannot be valid unless the validation element and keyset root FDA are the same.
        assert_eq!(
            super::_validate_create_keyset_root(&validate_data, &change_rule, &keyset_root),
            Error::AuthorNotFda.into(),
        );

        create_header.author = keyset_root.as_first_deepkey_agent_ref().clone();
        *validate_data.element.as_header_mut() = Header::Create(create_header);

        assert_eq!(
            Ok(ValidateCallbackResult::Valid),
            super::_validate_create_keyset_root(&validate_data, &change_rule, &keyset_root),
        );
    }

    #[test]
    fn test_validate_create_authorization() {
        let validate_data = fixt!(ValidateData);
        let mut change_rule = fixt!(ChangeRule);
        let keyset_root = fixt!(KeysetRoot);

        change_rule.spec_change.authorization_of_new_spec.push(fixt!(Authorization));
        change_rule.spec_change.authorization_of_new_spec.push(fixt!(Authorization));

        // Too many sigs fails.
        assert_eq!(
            super::_validate_create_authorization(&validate_data, &change_rule, &keyset_root),
            Error::MultipleCreateSignatures.into(),
        );

        change_rule.spec_change.authorization_of_new_spec = vec![];

        // No sig fails.
        assert_eq!(
            super::_validate_create_authorization(&validate_data, &change_rule, &keyset_root),
            Error::NoCreateSignature.into(),
        );

        // Invalid sig fails.
        let authorization = fixt!(Authorization);
        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_verify_signature()
            .with(mockall::predicate::eq(VerifySignature::new(
                keyset_root.as_root_pub_key_ref().clone(),
                authorization.1.clone(),
                change_rule.as_spec_change_ref().as_new_spec_ref().clone()
            ).unwrap()))
            .times(1)
            .return_const(Ok(false));

        hdk::prelude::set_hdk(mock_hdk);

        change_rule.spec_change.authorization_of_new_spec = vec![authorization];

        assert_eq!(
            super::_validate_create_authorization(&validate_data, &change_rule, &keyset_root),
            Error::BadCreateSignature.into(),
        );

        // Valid sig passes.
        let mut mock_hdk = hdk::prelude::MockHdkT::new();
        mock_hdk.expect_verify_signature()
            .times(1)
            .return_const(Ok(true));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            Ok(ValidateCallbackResult::Valid),
            super::_validate_create_authorization(&validate_data, &change_rule, &keyset_root),
        );
    }

    #[test]
    fn test_validate_update_keyset_root() {
        let validate_data = fixt!(ValidateData);
        let previous_change_rule = fixt!(ChangeRule);
        let mut proposed_change_rule = fixt!(ChangeRule);

        assert_eq!(
            super::_validate_update_keyset_root(&validate_data, &previous_change_rule, &proposed_change_rule),
            Error::KeysetRootMismatch.into(),
        );

        proposed_change_rule.keyset_root = previous_change_rule.keyset_root.clone();

        assert_eq!(
            super::_validate_update_keyset_root(&validate_data, &previous_change_rule, &proposed_change_rule),
            Ok(ValidateCallbackResult::Valid),
        )
    }

    #[test]
    fn test_validate_update_authorization() {
        let validate_data = fixt!(ValidateData);
        let mut previous_change_rule = fixt!(ChangeRule);
        let mut proposed_change_rule = fixt!(ChangeRule);

        // Add a couple of signatures to make tests easier to write.
        proposed_change_rule.spec_change.authorization_of_new_spec.push(fixt!(Authorization));
        proposed_change_rule.spec_change.authorization_of_new_spec.push(fixt!(Authorization));

        // Fewer signatures than required is a fail.
        previous_change_rule.spec_change.new_spec.sigs_required = proposed_change_rule.spec_change.authorization_of_new_spec.len() as u8 + 1;

        assert_eq!(
            super::_validate_update_authorization(&validate_data, &previous_change_rule, &proposed_change_rule),
            Error::WrongNumberOfSignatures.into(),
        );

        // More signatures than required is also a fail!
        previous_change_rule.spec_change.new_spec.sigs_required = proposed_change_rule.spec_change.authorization_of_new_spec.len() as u8 - 1;

        assert_eq!(
            super::_validate_update_authorization(&validate_data, &previous_change_rule, &proposed_change_rule),
            Error::WrongNumberOfSignatures.into(),
        );

        let three_signers: Vec<AgentPubKey> = AgentPubKeyFixturator::new(Predictable).take(3).collect();

        previous_change_rule.spec_change.new_spec.authorized_signers = three_signers.clone();
        previous_change_rule.spec_change.new_spec.sigs_required = 2;

        let two_signatures: Vec<Authorization> = vec![(0, fixt!(Signature)), (2, fixt!(Signature))];
        proposed_change_rule.spec_change.authorization_of_new_spec = two_signatures.clone();
        proposed_change_rule.spec_change.new_spec.sigs_required = 2;

        // Bad signatures is a fail.
        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_verify_signature()
            .with(mockall::predicate::eq(
                VerifySignature::new(
                    three_signers[0].clone().into(),
                    two_signatures[0].1.clone().into(),
                    proposed_change_rule.spec_change.new_spec.clone(),
                ).unwrap()
            ))
            .times(1)
            .return_const(Ok(true));

        mock_hdk.expect_verify_signature()
            .with(mockall::predicate::eq(
                VerifySignature::new(
                    three_signers[2].clone().into(),
                    two_signatures[1].1.clone().into(),
                    proposed_change_rule.spec_change.new_spec.clone(),
                ).unwrap()
            ))
            .times(1)
            .return_const(Ok(false));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_update_authorization(&validate_data, &previous_change_rule, &proposed_change_rule),
            Error::BadUpdateSignature.into(),
        );

        // All sigs valid = pass.
        let mut mock_hdk = hdk::prelude::MockHdkT::new();

        mock_hdk.expect_verify_signature()
            .with(mockall::predicate::eq(
                VerifySignature::new(
                    three_signers[0].clone().into(),
                    two_signatures[0].1.clone().into(),
                    proposed_change_rule.spec_change.new_spec.clone(),
                ).unwrap()
            ))
            .times(1)
            .return_const(Ok(true));

        mock_hdk.expect_verify_signature()
            .with(mockall::predicate::eq(
                VerifySignature::new(
                    three_signers[2].clone().into(),
                    two_signatures[1].1.clone().into(),
                    proposed_change_rule.spec_change.new_spec.clone(),
                ).unwrap()
            ))
            .times(1)
            .return_const(Ok(true));

        hdk::prelude::set_hdk(mock_hdk);

        assert_eq!(
            super::_validate_update_authorization(&validate_data, &previous_change_rule, &proposed_change_rule),
            Ok(ValidateCallbackResult::Valid),
        );
    }

    #[test]
    fn test_validate_update_spec() {
        let mut change_rule = fixt!(ChangeRule);

        assert_eq!(
            super::_validate_update_spec(&change_rule, &change_rule),
            Error::IdenticalUpdate.into(),
        );

        let mut different_change_rule = change_rule.clone();
        change_rule.spec_change.new_spec.sigs_required = 5;
        different_change_rule.spec_change.new_spec.sigs_required = 10;

        assert_eq!(
            super::_validate_update_spec(&change_rule, &different_change_rule),
            Ok(ValidateCallbackResult::Valid),
        );
    }
}