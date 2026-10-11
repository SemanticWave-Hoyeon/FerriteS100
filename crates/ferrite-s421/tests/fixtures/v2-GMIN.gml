<?xml version="1.0" encoding="UTF-8"?>
<Dataset xmlns:gml="http://www.opengis.net/gml/3.2"
    xmlns:s100_profile="http://www.iho.int/S-100/profile/s100_gmlProfile"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
    gml:id="S421.abc.abcde.00001"
    xmlns:S100="http://www.iho.int/s100gml/5.0"
    xmlns:S421="http://www.iec.ch/S421/2.0"
    xsi:schemaLocation="http://www.iec.ch/S421/2.0 ./S-421_schema_2.0.0.xsd"
    xmlns="http://www.iec.ch/S421/2.0"
>
    <gml:boundedBy>
        <gml:Envelope srsName="http://www.opengis.net/def/crs/EPSG/0/4326" srsDimension="2">
            <gml:lowerCorner>20.7860128 116.4340427</gml:lowerCorner>
            <gml:upperCorner>22.2442299 118.1042359</gml:upperCorner>
        </gml:Envelope>
    </gml:boundedBy>
    <S100:DatasetIdentificationInformation>
        <S100:encodingSpecification>S-100 Part 10b</S100:encodingSpecification>
        <S100:encodingSpecificationEdition>1.0</S100:encodingSpecificationEdition>
        <S100:productIdentifier>S-421</S100:productIdentifier>
        <S100:productEdition>2.0.0</S100:productEdition>
        <S100:applicationProfile>test</S100:applicationProfile>
        <S100:datasetFileIdentifier>421Minimum generic test data</S100:datasetFileIdentifier>
        <S100:datasetTitle>Minimum Test Route Dataset</S100:datasetTitle>
        <S100:datasetReferenceDate>2026-06-17</S100:datasetReferenceDate>
        <S100:datasetLanguage>eng</S100:datasetLanguage>
        <S100:datasetTopicCategory>transportation</S100:datasetTopicCategory>
        <S100:datasetPurpose>base</S100:datasetPurpose>
        <S100:updateNumber>0</S100:updateNumber>
    </S100:DatasetIdentificationInformation>
    <members>
        <!-- Schema minimum valid route baseline.
               Use the commented alternatives below to derive dedicated invalid-import samples.
               RouteSchedules and RouteWaypoints are intentionally retained in this repository copy
               to support combined import/export exercises; treat Route and RouteInfo as the strict
               normative minimum subset when deriving conformance-only cases. -->
        <Route gml:id="RTE">
            <!-- IEC63173-1 12.3.2.2 a) Invalid route version format. -->
            <!-- <routeFormatVersion>3.0.0</routeFormatVersion> -->
            <routeFormatVersion>2.0</routeFormatVersion>
            <!-- IEC63173-1 12.3.2.2 b) Duplicated route ID.
                  Reuse this routeID in a separately imported route without a new edition number. -->
            <routeID>550e8400-e29b-41d4-a716-446655440000</routeID>
            <routeEditionNo>1</routeEditionNo>
            <routeInfo xlink:href="#RTE.INFO" xlink:title="routeInfoComposition" />
        </Route>
        <RouteInfo gml:id="RTE.INFO">
            <routeInfoName>MinimumTestRoute</routeInfoName>
            <routeInfoAuthor>Korea VTS</routeInfoAuthor>
            <!-- IEC63173-1 12.3.2.2 c) Invalid time value in routeInfoEditionTime.
                  Uncomment the example below to trigger an invalid route info edition time error. -->
            <!-- <routeInfoEditionTime>2026-06-24T25:00:00Z</routeInfoEditionTime> -->
            <!-- IEC63173-1 12.3.2.2 d) Invalid enumeration value in routeInfoStatus. -->
            <!-- <routeInfoStatus>Invalid status enumeration value</routeInfoStatus> -->
            <routeInfoStatus>Initial</routeInfoStatus>
            <!-- IEC63173-1 12.3.2.2 e)-f) Optional validity range negative tests.
                  Add one invalid value below when preparing a dedicated invalid-import sample. -->
            <!-- <routeInfoValidityStart>2026-06-30T25:00:00Z</routeInfoValidityStart> -->
            <!-- <routeInfoValidityEnd>2026-06-30T30:00:00Z</routeInfoValidityEnd> -->
            <routeInfoCollection xlink:href="#RTE" xlink:title="routeInfoComposition" />
            <!-- IEC63173-1 12.3.2.2 g): bogus element retained on purpose to break schema validation. -->
            <!-- ThisIsABogusElement>This element should not validate</ThisIsABogusElement> -->
        </RouteInfo>
    </members>
</Dataset>