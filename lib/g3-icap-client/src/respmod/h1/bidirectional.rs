/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2023-2025 ByteDance and/or its affiliates.
 */

use std::sync::Arc;

use anyhow::anyhow;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite};

use g3_http::{H1BodyToChunkedTransfer, HttpBodyDecodeReader, HttpBodyReader};
use g3_io_ext::{IdleCheck, LimitedBufReadExt, StreamCopy, StreamCopyConfig, StreamCopyError};

use super::{
    H1RespmodAdaptationError, HttpAdaptedResponse, HttpResponseClientWriter,
    HttpResponseForAdaptation, RespmodAdaptationEndState, RespmodAdaptationRunState,
};
use crate::respmod::response::RespmodResponse;
use crate::{IcapClientReader, IcapClientWriter, IcapServiceClient};

pub(super) struct BidirectionalRecvIcapResponse<'a, I: IdleCheck> {
    pub(super) icap_client: &'a Arc<IcapServiceClient>,
    pub(super) icap_reader: &'a mut IcapClientReader,
    pub(super) idle_checker: &'a I,
}

impl<I: IdleCheck> BidirectionalRecvIcapResponse<'_, I> {
    pub(super) async fn transfer_and_recv<UR>(
        self,
        mut body_transfer: &mut H1BodyToChunkedTransfer<'_, UR, IcapClientWriter>,
    ) -> Result<RespmodResponse, H1RespmodAdaptationError>
    where
        UR: AsyncBufRead + Unpin,
    {
        let mut idle_interval = self.idle_checker.interval_timer();
        let mut idle_count = 0;

        loop {
            tokio::select! {
                biased;

                r = &mut body_transfer => {
                    return match r {
                        Ok(_) => self.recv_icap_response().await,
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1RespmodAdaptationError::HttpUpstreamReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1RespmodAdaptationError::IcapServerWriteFailed(e)),
                    };
                }
                r = self.icap_reader.fill_wait_data() => {
                    return match r {
                        Ok(true) => self.recv_icap_response().await,
                        Ok(false) => Err(H1RespmodAdaptationError::IcapServerConnectionClosed),
                        Err(e) => Err(H1RespmodAdaptationError::IcapServerReadFailed(e)),
                    };
                }
                n = idle_interval.tick() => {
                    if body_transfer.is_idle() {
                        idle_count += n;

                        let quit = self.idle_checker.check_quit(idle_count);
                        if quit {
                            return if body_transfer.no_cached_data() {
                                Err(H1RespmodAdaptationError::HttpUpstreamReadIdle)
                            } else {
                                Err(H1RespmodAdaptationError::IcapServerWriteIdle)
                            };
                        }
                    } else {
                        idle_count = 0;

                        body_transfer.reset_active();
                    }

                    if let Some(reason) = self.idle_checker.check_force_quit() {
                        return Err(H1RespmodAdaptationError::IdleForceQuit(reason));
                    }
                }
            }
        }
    }

    async fn recv_icap_response(self) -> Result<RespmodResponse, H1RespmodAdaptationError> {
        let rsp = RespmodResponse::parse(
            self.icap_reader,
            self.icap_client.config.icap_max_header_size,
        )
        .await?;
        Ok(rsp)
    }
}

pub(super) struct BidirectionalRecvHttpResponse<'a, I: IdleCheck> {
    pub(super) http_body_line_max_size: usize,
    pub(super) copy_config: StreamCopyConfig,
    pub(super) idle_checker: &'a I,
    pub(super) http_header_size: usize,
    pub(super) icap_read_finished: bool,
}

impl<I: IdleCheck> BidirectionalRecvHttpResponse<'_, I> {
    pub(super) async fn transfer<H, UR, CW>(
        &mut self,
        state: &mut RespmodAdaptationRunState,
        ups_body_transfer: &mut H1BodyToChunkedTransfer<'_, UR, IcapClientWriter>,
        orig_http_response: &H,
        icap_reader: &mut IcapClientReader,
        clt_writer: &mut CW,
    ) -> Result<RespmodAdaptationEndState<H>, H1RespmodAdaptationError>
    where
        H: HttpResponseForAdaptation,
        UR: AsyncBufRead + Unpin,
        CW: HttpResponseClientWriter<H> + Unpin,
    {
        let http_rsp = HttpAdaptedResponse::parse(icap_reader, self.http_header_size).await?;
        let body_content_length = http_rsp.content_length;

        let final_rsp = orig_http_response.adapt_with_body(http_rsp);
        state.mark_clt_send_start();
        clt_writer
            .send_response_header(&final_rsp)
            .await
            .map_err(H1RespmodAdaptationError::HttpClientWriteFailed)?;
        state.mark_clt_send_header();

        match body_content_length {
            Some(0) => Err(H1RespmodAdaptationError::InvalidHttpBodyFromIcapServer(
                anyhow!("Content-Length is 0 but the ICAP server response contains http-body"),
            )),
            Some(expected) => {
                let mut clt_body_reader =
                    HttpBodyDecodeReader::new_chunked(icap_reader, self.http_body_line_max_size);
                let mut clt_body_transfer =
                    StreamCopy::new(&mut clt_body_reader, clt_writer, &self.copy_config);
                self.do_transfer(ups_body_transfer, &mut clt_body_transfer)
                    .await?;

                state.mark_clt_send_all();
                let copied = clt_body_transfer.copied_size();
                if clt_body_reader.trailer(128).await.is_ok() {
                    self.icap_read_finished = true;
                }

                if copied != expected {
                    return Err(H1RespmodAdaptationError::InvalidHttpBodyFromIcapServer(
                        anyhow!("Content-Length is {expected} but decoded length is {copied}"),
                    ));
                }
                Ok(RespmodAdaptationEndState::AdaptedTransferred(final_rsp))
            }
            None => {
                let mut clt_body_reader =
                    HttpBodyReader::new_chunked(icap_reader, self.http_body_line_max_size);
                let mut clt_body_transfer =
                    StreamCopy::new(&mut clt_body_reader, clt_writer, &self.copy_config);
                self.do_transfer(ups_body_transfer, &mut clt_body_transfer)
                    .await?;

                state.mark_clt_send_all();
                self.icap_read_finished = clt_body_transfer.finished();

                Ok(RespmodAdaptationEndState::AdaptedTransferred(final_rsp))
            }
        }
    }

    async fn do_transfer<UR, IR, CW>(
        &self,
        mut ups_body_transfer: &mut H1BodyToChunkedTransfer<'_, UR, IcapClientWriter>,
        mut clt_body_transfer: &mut StreamCopy<'_, IR, CW>,
    ) -> Result<(), H1RespmodAdaptationError>
    where
        UR: AsyncBufRead + Unpin,
        IR: AsyncRead + Unpin,
        CW: AsyncWrite + Unpin,
    {
        let mut idle_interval = self.idle_checker.interval_timer();
        let mut idle_count = 0;

        loop {
            tokio::select! {
                r = &mut ups_body_transfer => {
                    return match r {
                        Ok(_) => {
                            match clt_body_transfer.await {
                                Ok(_) => Ok(()),
                                Err(StreamCopyError::ReadFailed(e)) => Err(H1RespmodAdaptationError::IcapServerReadFailed(e)),
                                Err(StreamCopyError::WriteFailed(e)) => Err(H1RespmodAdaptationError::HttpClientWriteFailed(e)),
                            }
                        }
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1RespmodAdaptationError::HttpUpstreamReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1RespmodAdaptationError::IcapServerWriteFailed(e)),
                    };
                }
                r = &mut clt_body_transfer => {
                    return match r {
                        Ok(_) => Ok(()),
                        Err(StreamCopyError::ReadFailed(e)) => Err(H1RespmodAdaptationError::IcapServerReadFailed(e)),
                        Err(StreamCopyError::WriteFailed(e)) => Err(H1RespmodAdaptationError::HttpClientWriteFailed(e)),
                    };
                }
                n = idle_interval.tick() => {
                    if ups_body_transfer.is_idle() && clt_body_transfer.is_idle() {
                        idle_count += n;

                        let quit = self.idle_checker.check_quit(idle_count);
                        if quit {
                            return if ups_body_transfer.is_idle() {
                                if ups_body_transfer.no_cached_data() {
                                    Err(H1RespmodAdaptationError::HttpUpstreamReadIdle)
                                } else {
                                    Err(H1RespmodAdaptationError::IcapServerWriteIdle)
                                }
                            } else if clt_body_transfer.no_cached_data() {
                                Err(H1RespmodAdaptationError::IcapServerReadIdle)
                            } else {
                                Err(H1RespmodAdaptationError::HttpClientWriteIdle)
                            };
                        }
                    } else {
                        idle_count = 0;

                        ups_body_transfer.reset_active();
                        clt_body_transfer.reset_active();
                    }

                    if let Some(reason) = self.idle_checker.check_force_quit() {
                        return Err(H1RespmodAdaptationError::IdleForceQuit(reason));
                    }
                }
            }
        }
    }
}
