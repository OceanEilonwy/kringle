//! Kringle addition: send one plain mail from the logged-in Tuta account.
//!
//! Upstream's Rust SDK can log in and read mail but has no send path, so this
//! follows the web client's `MailFacade.createDraft` + `sendDraft`
//! (src/applications/common/api/worker/facades/lazy/MailFacade.ts) for the
//! simplest case: one recipient, no attachments, no reply.
//!
//! - A Tuta recipient (the server has a public key for the address) gets an
//!   end-to-end encrypted mail: the mail's session key is wrapped in a bucket
//!   key, and the bucket key is encrypted for the recipient's public key.
//! - Anyone else gets a normal (non-confidential) mail: the session key is
//!   handed to the server so it can deliver the mail over SMTP.

use crate::crypto::public_key_provider::{PublicKeyIdentifier, PublicKeyLoadingError};
use crate::crypto::Aes256Key;
use crate::entities::generated::tutanota::{
	BodyTransferAggregatedType, DraftCreateData, DraftData, InternalRecipientKeyData,
	MailAddressTransferAggregatedType, MailDetailsBlobTransferAggregatedType,
	MailDetailsTransferAggregatedType, MailTransferAggregatedType,
	RecipientsTransferAggregatedType, SendDraftData, SendDraftParameters,
};
use crate::groups::GroupType;
use crate::rest_error::HttpError;
use crate::services::generated::tutanota::{DraftService, SendDraftService};
use crate::services::ExtraServiceParams;
use crate::tutanota_constants::{
	CryptoProtocolVersion, EncryptionAuthStatus, PublicKeyIdentifierType,
};
use crate::{ApiCallError, LoggedInSdk};
use crypto_primitives::aes::InitializationVector;
use crypto_primitives::key::GenericAesKey;
use crypto_primitives::randomizer_facade::RandomizerFacade;

/// Why a mail could not be sent. The text is meant for a log line.
#[derive(thiserror::Error, Debug)]
pub enum SendMailError {
	#[error("{0}")]
	Api(#[from] ApiCallError),
	#[error("could not encrypt the mail for {address}: {reason}")]
	Encrypt { address: String, reason: String },
}

const CONVERSATION_NEW: i64 = 0;
const MAIL_METHOD_NONE: i64 = 0;

fn address(name: &str, address: &str) -> MailAddressTransferAggregatedType {
	MailAddressTransferAggregatedType {
		_id: None,
		name: name.to_string(),
		address: address.to_string(),
		contact: None,
		_errors: Default::default(),
	}
}

impl LoggedInSdk {
	/// Sends one mail with an HTML body from `sender` (an address of the
	/// logged-in account) to one recipient.
	pub async fn send_mail(
		&self,
		sender: &str,
		sender_name: &str,
		to_name: &str,
		to: &str,
		subject: &str,
		html_body: &str,
	) -> Result<(), SendMailError> {
		let randomizer = RandomizerFacade::from_core(rand_core::OsRng);
		let encrypt_err = |reason: String| SendMailError::Encrypt {
			address: to.to_string(),
			reason,
		};

		// Is the recipient a Tuta user? Then the mail is end-to-end encrypted.
		let recipient_key = match self
			.public_key_provider
			.load_current_pub_key(&PublicKeyIdentifier {
				identifier: to.to_string(),
				identifier_type: PublicKeyIdentifierType::MailAddress,
			})
			.await
		{
			Ok(key) => Some(key),
			Err(PublicKeyLoadingError::KeyLoadingError(ApiCallError::ServerResponseError {
				source: HttpError::NotFoundError,
			})) => None,
			Err(PublicKeyLoadingError::KeyLoadingError(e)) => return Err(e.into()),
			Err(PublicKeyLoadingError::KeyValidationError(e)) => return Err(encrypt_err(e)),
		};
		let confidential = recipient_key.is_some();

		// 1. The draft, encrypted with a fresh session key that the mailbox
		//    owner can unwrap with the mail group key.
		let mail_group = self
			.user_facade
			.get_membership_by_group_type(GroupType::Mail)?
			.group;
		let mail_group_key = self
			.key_loader_facade
			.get_current_sym_group_key(&mail_group)
			.await
			.map_err(|e| encrypt_err(format!("mail group key: {e:?}")))?;
		let session_key = GenericAesKey::Aes256(Aes256Key::generate(&randomizer));
		let owner_enc_session_key = mail_group_key
			.object
			.encrypt_key(&session_key, InitializationVector::generate(&randomizer));
		let owner_key_version = mail_group_key.version as i64;

		let draft = DraftCreateData {
			_format: 0,
			previousMessageId: None,
			conversationType: CONVERSATION_NEW,
			ownerEncSessionKey: None,
			ownerKeyVersion: None,
			draftData: DraftData {
				_id: None,
				subject: None,
				bodyText: None,
				senderMailAddress: None,
				senderName: None,
				confidential: None,
				method: None,
				compressedBodyText: None,
				toRecipients: vec![],
				ccRecipients: vec![],
				bccRecipients: vec![],
				addedAttachments: vec![],
				removedAttachments: vec![],
				replyTos: vec![],
				mail: Some(MailTransferAggregatedType {
					_id: None,
					_ownerEncSessionKey: Some(owner_enc_session_key.clone()),
					_ownerKeyVersion: Some(owner_key_version),
					_kdfNonce: None,
					subject: subject.to_string(),
					confidential,
					method: MAIL_METHOD_NONE,
					sender: address(sender_name, sender),
					firstRecipient: Some(address(to_name, to)),
					_errors: Default::default(),
				}),
				mailDetailsBlob: Some(MailDetailsBlobTransferAggregatedType {
					_id: None,
					_ownerEncSessionKey: Some(owner_enc_session_key),
					_ownerKeyVersion: Some(owner_key_version),
					_kdfNonce: None,
					details: MailDetailsTransferAggregatedType {
						_id: None,
						replyTos: vec![],
						recipients: RecipientsTransferAggregatedType {
							_id: None,
							toRecipients: vec![address(to_name, to)],
							ccRecipients: vec![],
							bccRecipients: vec![],
						},
						body: BodyTransferAggregatedType {
							_id: None,
							text: Some(String::new()),
							compressedText: Some(html_body.to_string()),
							_errors: Default::default(),
						},
					},
				}),
				_errors: Default::default(),
			},
			_errors: Default::default(),
		};
		let created = self
			.service_executor
			.post::<DraftService>(
				draft,
				ExtraServiceParams {
					session_key: Some(session_key.clone()),
					..Default::default()
				},
			)
			.await?;

		// 2. Send it, with the session key either sealed for the Tuta
		//    recipient or handed to the server for delivery.
		let mut mail_session_key = None;
		let mut bucket_enc_mail_session_key = None;
		let mut internal = vec![];
		let mut auth_status = None;
		if let Some(recipient_key) = recipient_key {
			let bucket_key = GenericAesKey::Aes256(Aes256Key::generate(&randomizer));
			bucket_enc_mail_session_key = Some(
				bucket_key.encrypt_key(&session_key, InitializationVector::generate(&randomizer)),
			);
			let sealed = self
				.asymmetric_crypto_facade
				.asym_encrypt_sym_key(
					bucket_key,
					recipient_key,
					&self.user_facade.get_user_group_id(),
				)
				.await
				.map_err(|e| encrypt_err(format!("{e:?}")))?;
			if sealed.crypto_protocol_version == CryptoProtocolVersion::TutaCrypt {
				let status = (EncryptionAuthStatus::TutacryptSender as i64).to_string();
				auth_status = Some(
					session_key
						.encrypt_data(
							status.as_bytes(),
							InitializationVector::generate(&randomizer),
						)
						.map_err(|e| encrypt_err(format!("{e:?}")))?,
				);
			}
			internal.push(InternalRecipientKeyData {
				_id: None,
				mailAddress: to.to_string(),
				pubEncBucketKey: sealed.pub_enc_sym_key_bytes,
				recipientKeyVersion: sealed.recipient_key_version as i64,
				protocolVersion: sealed.crypto_protocol_version as i64,
				senderKeyVersion: sealed.sender_key_version.map(|v| v as i64),
			});
		} else {
			mail_session_key = Some(session_key.as_bytes().to_vec());
		}

		let language = "en".to_string();
		let send = SendDraftData {
			_format: 0,
			language: language.clone(),
			mailSessionKey: mail_session_key.clone(),
			bucketEncMailSessionKey: bucket_enc_mail_session_key.clone(),
			senderNameUnencrypted: None,
			plaintext: false,
			calendarMethod: false,
			sessionEncEncryptionAuthStatus: auth_status.clone(),
			sendAt: None,
			allowUndo: false,
			internalRecipientKeyData: internal.clone(),
			secureExternalRecipientKeyData: vec![],
			attachmentKeyData: vec![],
			mail: created.draft.clone(),
			symEncInternalRecipientKeyData: vec![],
			parameters: Some(SendDraftParameters {
				_id: None,
				language,
				mailSessionKey: mail_session_key,
				bucketEncMailSessionKey: bucket_enc_mail_session_key,
				senderNameUnencrypted: None,
				plaintext: false,
				calendarMethod: false,
				sessionEncEncryptionAuthStatus: auth_status,
				mail: created.draft,
				internalRecipientKeyData: internal,
				secureExternalRecipientKeyData: vec![],
				symEncInternalRecipientKeyData: vec![],
				attachmentKeyData: vec![],
			}),
		};
		self.service_executor
			.post::<SendDraftService>(send, ExtraServiceParams::default())
			.await?;
		Ok(())
	}
}
